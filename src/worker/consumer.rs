//! Durable Iggy consumption for queued UserOperations.
//!
//! Every `chain-{id}` stream gets one consumer group. All relay replicas use the same group
//! name, so Iggy assigns the stream's current single partition to only one dispatcher at a time.
//! A dispatcher fans a polled batch out to ten deterministic EOA lanes and advances the group
//! offset only across the contiguous prefix whose handlers reported durable success.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    error::Error,
    fmt::{Display, Formatter},
    future::Future,
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use iggy::prelude::{
    Client, Consumer, ConsumerGroupClient, ConsumerOffsetClient, Identifier, IggyClient, IggyError,
    MessageClient, PollingStrategy, StreamClient,
};
use serde::Deserialize;
use serde_json::Value;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::MissedTickBehavior,
};
use tokio_util::sync::CancellationToken;

pub const RELAYER_LANE_COUNT: u8 = 10;
pub const USER_OPERATION_TOPIC: &str = "default";
pub const DEFAULT_CONSUMER_GROUP: &str = "vela-relay-user-operations-v1";

const DEFAULT_DISCOVERY_INTERVAL: Duration = Duration::from_secs(15);
const DEFAULT_EMPTY_POLL_INTERVAL: Duration = Duration::from_millis(100);
const DEFAULT_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(30);
const DEFAULT_BATCH_SIZE: u32 = 100;
const MAX_BATCH_SIZE: u32 = 10_000;

pub type UserOperationHandlerError = Box<dyn Error + Send + Sync + 'static>;
pub type UserOperationBatchResults = Vec<Result<(), UserOperationHandlerError>>;
pub type UserOperationHandlerFuture<'a> =
    Pin<Box<dyn Future<Output = UserOperationBatchResults> + Send + 'a>>;
pub type MalformedUserOperationHandlerFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), UserOperationHandlerError>> + Send + 'a>>;

/// Executes lane-routed UserOperations in handleOps-sized batches.
///
/// Every `handle_batch` call contains operations from exactly one chain and one lane, ordered by
/// Iggy offset. The returned vector must have the same length and order as the input. Each
/// `Ok(())` independently confirms that operation reached a durable outcome (including a durable
/// rejection); an error keeps that operation's offset uncommitted. Calls are sequential within one
/// lane and may run concurrently across different lanes.
///
/// `handle_malformed` must persist a rejection or dead-letter record before returning `Ok(())`.
/// Implementations must be idempotent because a process can fail after a durable effect but before
/// the Iggy offset write.
pub trait UserOperationHandler: Send + Sync + 'static {
    fn handle_batch(&self, operations: Vec<RoutedUserOperation>) -> UserOperationHandlerFuture<'_>;

    fn handle_malformed(
        &self,
        operation: MalformedUserOperation,
    ) -> MalformedUserOperationHandlerFuture<'_>;
}

pub use vela_relay_core::task::RoutedUserOperation;

/// A queue message that cannot be safely routed to an EOA lane.
///
/// The raw payload is retained so a handler can write a lossless dead-letter record. The hash is
/// best-effort because malformed JSON or a missing field may make it unavailable.
#[derive(Clone, Debug)]
pub struct MalformedUserOperation {
    pub chain_id: u64,
    pub stream: String,
    pub partition_id: u32,
    pub offset: u64,
    pub user_operation_hash: Option<String>,
    pub payload: Vec<u8>,
    pub error: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChainStream {
    pub chain_id: u64,
    pub name: String,
}

/// Consumer settings kept separate from application configuration so the worker can be wired in
/// without coupling the queue protocol to environment parsing.
#[derive(Clone)]
pub struct UserOperationConsumerConfig {
    pub connection_string: String,
    pub consumer_group: String,
    pub discovery_interval: Duration,
    pub empty_poll_interval: Duration,
    pub retry_interval: Duration,
    pub batch_size: u32,
}

impl UserOperationConsumerConfig {
    pub fn new(connection_string: impl Into<String>) -> Self {
        Self {
            connection_string: connection_string.into(),
            consumer_group: DEFAULT_CONSUMER_GROUP.into(),
            discovery_interval: DEFAULT_DISCOVERY_INTERVAL,
            empty_poll_interval: DEFAULT_EMPTY_POLL_INTERVAL,
            retry_interval: DEFAULT_RETRY_INTERVAL,
            batch_size: DEFAULT_BATCH_SIZE,
        }
    }

    fn validate(&self) -> Result<(), UserOperationConsumerError> {
        if self.connection_string.trim().is_empty() {
            return Err(UserOperationConsumerError::new(
                "Iggy consumer connection string is empty",
            ));
        }
        if self.consumer_group.trim().is_empty() {
            return Err(UserOperationConsumerError::new(
                "Iggy consumer group name is empty",
            ));
        }
        if Identifier::try_from(self.consumer_group.as_str()).is_err() {
            return Err(UserOperationConsumerError::new(
                "Iggy consumer group name is invalid",
            ));
        }
        if self.discovery_interval.is_zero()
            || self.empty_poll_interval.is_zero()
            || self.retry_interval.is_zero()
        {
            return Err(UserOperationConsumerError::new(
                "Iggy consumer intervals must be greater than zero",
            ));
        }
        if self.batch_size == 0 || self.batch_size > MAX_BATCH_SIZE {
            return Err(UserOperationConsumerError::new(
                "Iggy consumer batch size must be between 1 and 10000",
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct UserOperationConsumerError {
    message: String,
    session: bool,
}

impl UserOperationConsumerError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            session: false,
        }
    }

    fn iggy(action: &'static str, error: IggyError) -> Self {
        Self {
            message: format!("{action}: {error}"),
            session: crate::utils::iggy::is_session_error(&error),
        }
    }

    /// True when the shared Iggy client's session is dead (dropped, deauthenticated, or forgotten
    /// by the server) and no retry on the same client can succeed — only a fresh connection can.
    pub fn is_session_error(&self) -> bool {
        self.session
    }
}

impl Display for UserOperationConsumerError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for UserOperationConsumerError {}

/// Discovers `chain-{u64}` streams and owns one shared dispatcher group per stream.
pub struct UserOperationConsumer {
    client: Arc<IggyClient>,
    config: UserOperationConsumerConfig,
    handler: Arc<dyn UserOperationHandler>,
}

impl UserOperationConsumer {
    pub async fn connect(
        config: UserOperationConsumerConfig,
        handler: Arc<dyn UserOperationHandler>,
    ) -> Result<Self, UserOperationConsumerError> {
        config.validate()?;
        let client = connect_client(&config).await?;

        Ok(Self {
            client: Arc::new(client),
            config,
            handler,
        })
    }

    /// Returns the current strictly named chain streams, sorted by chain ID.
    pub async fn discover_chain_streams(
        &self,
    ) -> Result<Vec<ChainStream>, UserOperationConsumerError> {
        let streams = self.client.get_streams().await.map_err(|error| {
            UserOperationConsumerError::iggy("could not list Iggy streams", error)
        })?;
        Ok(filter_chain_streams(
            streams.into_iter().map(|stream| stream.name),
        ))
    }

    /// Starts from an already-discovered topology, avoiding a duplicate metadata request during
    /// worker readiness initialization.
    pub async fn run_with_discovered_chain_streams(
        mut self,
        streams: Vec<ChainStream>,
        shutdown: CancellationToken,
    ) -> Result<(), UserOperationConsumerError> {
        let mut active = HashMap::<u64, StreamDispatcher>::new();
        let mut retiring = Vec::<JoinHandle<()>>::new();
        reconcile_dispatchers(
            &mut active,
            &mut retiring,
            streams,
            self.client.clone(),
            self.config.clone(),
            self.handler.clone(),
            &shutdown,
        );

        let mut discovery = tokio::time::interval(self.config.discovery_interval);
        discovery.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // The topology immediately above is the first interval tick's work.
        discovery.tick().await;

        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                _ = discovery.tick() => {
                    reap_finished_dispatchers(&mut active, &mut retiring).await;
                    match self.discover_chain_streams().await {
                        Ok(streams) => reconcile_dispatchers(
                            &mut active,
                            &mut retiring,
                            streams,
                            self.client.clone(),
                            self.config.clone(),
                            self.handler.clone(),
                            &shutdown,
                        ),
                        Err(error) if error.is_session_error() => {
                            tracing::warn!(
                                %error,
                                "Iggy consumer session is dead, rebuilding the connection"
                            );
                            if self.rebuild_connection(&mut active, &mut retiring, &shutdown).await {
                                match self.discover_chain_streams().await {
                                    Ok(streams) => reconcile_dispatchers(
                                        &mut active,
                                        &mut retiring,
                                        streams,
                                        self.client.clone(),
                                        self.config.clone(),
                                        self.handler.clone(),
                                        &shutdown,
                                    ),
                                    Err(error) => tracing::warn!(
                                        %error,
                                        "Iggy chain stream discovery failed after reconnect"
                                    ),
                                }
                            }
                        }
                        Err(error) => {
                            tracing::warn!(%error, "Iggy chain stream discovery failed");
                        }
                    }
                }
            }
        }

        for (_, dispatcher) in active.drain() {
            dispatcher.shutdown.cancel();
            retiring.push(dispatcher.task);
        }
        for task in retiring {
            if let Err(error) = task.await {
                tracing::warn!(?error, "Iggy stream dispatcher stopped unexpectedly");
            }
        }

        if let Err(error) = self.client.shutdown().await {
            tracing::warn!(%error, "could not cleanly shut down Iggy consumer connection");
        }
        Ok(())
    }

    /// Replaces a dead shared client with a freshly connected one.
    ///
    /// Every dispatcher holds a clone of the dead client, so they are all stopped first; the
    /// caller re-reconciles them against the new client afterwards. Connection attempts retry
    /// with capped exponential backoff until they succeed or shutdown is requested. Returns
    /// false only when shutdown interrupted the rebuild.
    async fn rebuild_connection(
        &mut self,
        active: &mut HashMap<u64, StreamDispatcher>,
        retiring: &mut Vec<JoinHandle<()>>,
        shutdown: &CancellationToken,
    ) -> bool {
        for (_, dispatcher) in active.drain() {
            dispatcher.shutdown.cancel();
            retiring.push(dispatcher.task);
        }
        for task in retiring.drain(..) {
            if let Err(error) = task.await {
                tracing::warn!(?error, "Iggy stream dispatcher stopped unexpectedly");
            }
        }
        if let Err(error) = self.client.shutdown().await {
            tracing::debug!(%error, "could not cleanly shut down the dead Iggy consumer client");
        }

        let mut backoff = self.config.retry_interval;
        loop {
            let connected = tokio::select! {
                _ = shutdown.cancelled() => return false,
                result = connect_client(&self.config) => result,
            };
            match connected {
                Ok(client) => {
                    self.client = Arc::new(client);
                    tracing::info!("Iggy consumer connection re-established");
                    return true;
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        retry_in_secs = backoff.as_secs(),
                        "could not re-establish Iggy consumer connection"
                    );
                }
            }
            tokio::select! {
                _ = shutdown.cancelled() => return false,
                _ = tokio::time::sleep(backoff) => {}
            }
            backoff = (backoff * 2).min(MAX_RECONNECT_BACKOFF);
        }
    }
}

async fn connect_client(
    config: &UserOperationConsumerConfig,
) -> Result<IggyClient, UserOperationConsumerError> {
    let client =
        IggyClient::from_connection_string(&config.connection_string).map_err(|error| {
            UserOperationConsumerError::iggy("invalid Iggy consumer connection", error)
        })?;
    client.connect().await.map_err(|error| {
        UserOperationConsumerError::iggy("could not connect Iggy consumer", error)
    })?;
    Ok(client)
}

struct StreamDispatcher {
    stream: String,
    shutdown: CancellationToken,
    task: JoinHandle<()>,
}

fn reconcile_dispatchers(
    active: &mut HashMap<u64, StreamDispatcher>,
    retiring: &mut Vec<JoinHandle<()>>,
    streams: Vec<ChainStream>,
    client: Arc<IggyClient>,
    config: UserOperationConsumerConfig,
    handler: Arc<dyn UserOperationHandler>,
    shutdown: &CancellationToken,
) {
    let discovered = streams
        .iter()
        .map(|stream| stream.chain_id)
        .collect::<BTreeSet<_>>();
    let removed = active
        .keys()
        .copied()
        .filter(|chain_id| !discovered.contains(chain_id))
        .collect::<Vec<_>>();
    let mut changed = false;

    for chain_id in removed {
        if let Some(dispatcher) = active.remove(&chain_id) {
            dispatcher.shutdown.cancel();
            retiring.push(dispatcher.task);
            tracing::info!(chain_id, stream = %dispatcher.stream, "retiring Iggy chain dispatcher");
            changed = true;
        }
    }

    for stream in streams {
        if active.contains_key(&stream.chain_id) {
            continue;
        }

        let dispatcher_shutdown = shutdown.child_token();
        let task = tokio::spawn(run_stream_dispatcher(
            client.clone(),
            config.clone(),
            handler.clone(),
            stream.clone(),
            dispatcher_shutdown.clone(),
        ));
        tracing::info!(
            chain_id = stream.chain_id,
            stream = %stream.name,
            lanes = RELAYER_LANE_COUNT,
            group = %config.consumer_group,
            "started Iggy chain dispatcher"
        );
        active.insert(
            stream.chain_id,
            StreamDispatcher {
                stream: stream.name,
                shutdown: dispatcher_shutdown,
                task,
            },
        );
        changed = true;
    }

    if changed {
        let mut names = active
            .values()
            .map(|dispatcher| dispatcher.stream.as_str())
            .collect::<Vec<_>>();
        names.sort_unstable();
        tracing::info!(count = names.len(), streams = ?names, "Iggy chain dispatchers reconciled");
    }
}

async fn reap_finished_dispatchers(
    active: &mut HashMap<u64, StreamDispatcher>,
    retiring: &mut Vec<JoinHandle<()>>,
) {
    let finished = active
        .iter()
        .filter_map(|(chain_id, dispatcher)| dispatcher.task.is_finished().then_some(*chain_id))
        .collect::<Vec<_>>();
    for chain_id in finished {
        if let Some(dispatcher) = active.remove(&chain_id) {
            retiring.push(dispatcher.task);
        }
    }

    let mut index = 0;
    while index < retiring.len() {
        if retiring[index].is_finished() {
            let task = retiring.swap_remove(index);
            if let Err(error) = task.await {
                tracing::warn!(?error, "Iggy stream dispatcher stopped unexpectedly");
            }
        } else {
            index += 1;
        }
    }
}

async fn run_stream_dispatcher(
    client: Arc<IggyClient>,
    config: UserOperationConsumerConfig,
    handler: Arc<dyn UserOperationHandler>,
    stream: ChainStream,
    shutdown: CancellationToken,
) {
    loop {
        if shutdown.is_cancelled() {
            return;
        }

        match consume_stream_session(&client, &config, handler.clone(), &stream, &shutdown).await {
            Ok(()) => return,
            Err(error) => tracing::warn!(
                chain_id = stream.chain_id,
                stream = %stream.name,
                %error,
                "Iggy chain dispatcher session failed"
            ),
        }

        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(config.retry_interval) => {}
        }
    }
}

async fn consume_stream_session(
    client: &IggyClient,
    config: &UserOperationConsumerConfig,
    handler: Arc<dyn UserOperationHandler>,
    stream: &ChainStream,
    shutdown: &CancellationToken,
) -> Result<(), UserOperationConsumerError> {
    let stream_id: Identifier = stream.name.as_str().try_into().map_err(|error| {
        UserOperationConsumerError::iggy("invalid Iggy chain stream name", error)
    })?;
    let topic_id: Identifier = USER_OPERATION_TOPIC.try_into().map_err(|error| {
        UserOperationConsumerError::iggy("invalid Iggy UserOperation topic name", error)
    })?;
    let group_id: Identifier = config.consumer_group.as_str().try_into().map_err(|error| {
        UserOperationConsumerError::iggy("invalid Iggy consumer group name", error)
    })?;
    let consumer = Consumer::group(group_id.clone());

    ensure_consumer_group(
        client,
        &stream_id,
        &topic_id,
        &group_id,
        &config.consumer_group,
    )
    .await?;
    client
        .join_consumer_group(&stream_id, &topic_id, &group_id)
        .await
        .map_err(|error| {
            UserOperationConsumerError::iggy("could not join Iggy consumer group", error)
        })?;

    let lanes = LanePool::start(handler);
    let consume_result = consume_joined_group(
        client, config, stream, &stream_id, &topic_id, &consumer, &lanes, shutdown,
    )
    .await;
    lanes.shutdown().await;

    if let Err(error) = client
        .leave_consumer_group(&stream_id, &topic_id, &group_id)
        .await
    {
        tracing::warn!(
            chain_id = stream.chain_id,
            stream = %stream.name,
            %error,
            "could not leave Iggy consumer group"
        );
    }
    consume_result
}

async fn ensure_consumer_group(
    client: &IggyClient,
    stream: &Identifier,
    topic: &Identifier,
    group: &Identifier,
    group_name: &str,
) -> Result<(), UserOperationConsumerError> {
    let exists = client
        .get_consumer_group(stream, topic, group)
        .await
        .map_err(|error| {
            UserOperationConsumerError::iggy("could not inspect Iggy consumer group", error)
        })?
        .is_some();
    if exists {
        return Ok(());
    }

    if let Err(create_error) = client
        .create_consumer_group(stream, topic, group_name)
        .await
    {
        // Creation is intentionally race-safe across replicas: another replica may have won
        // between the lookup and create calls.
        let was_created_concurrently = client
            .get_consumer_group(stream, topic, group)
            .await
            .map_err(|error| {
                UserOperationConsumerError::iggy("could not recheck Iggy consumer group", error)
            })?
            .is_some();
        if !was_created_concurrently {
            return Err(UserOperationConsumerError::iggy(
                "could not create Iggy consumer group",
                create_error,
            ));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn consume_joined_group(
    client: &IggyClient,
    config: &UserOperationConsumerConfig,
    stream: &ChainStream,
    stream_id: &Identifier,
    topic_id: &Identifier,
    consumer: &Consumer,
    lanes: &LanePool,
    shutdown: &CancellationToken,
) -> Result<(), UserOperationConsumerError> {
    let polling_strategy = PollingStrategy::next();

    loop {
        let polled = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return Ok(()),
            result = client.poll_messages(
                stream_id,
                topic_id,
                None,
                consumer,
                &polling_strategy,
                config.batch_size,
                false,
            ) => result.map_err(|error| {
                UserOperationConsumerError::iggy("could not poll Iggy UserOperations", error)
            })?,
        };

        if polled.messages.is_empty() {
            tokio::select! {
                _ = shutdown.cancelled() => return Ok(()),
                _ = tokio::time::sleep(config.empty_poll_interval) => {}
            }
            continue;
        }

        let partition_id = polled.partition_id;
        let batch = lanes
            .process_batch(stream, partition_id, polled.messages)
            .await;

        if let Some(offset) = batch.highest_contiguous_durable_offset {
            client
                .store_consumer_offset(consumer, stream_id, topic_id, Some(partition_id), offset)
                .await
                .map_err(|error| {
                    UserOperationConsumerError::iggy(
                        "could not store durable Iggy consumer offset",
                        error,
                    )
                })?;
        }

        if batch.fatal_lane_failure {
            return Err(UserOperationConsumerError::new(
                "a UserOperation relayer lane stopped or violated its result contract",
            ));
        }

        if batch.had_failure {
            tokio::select! {
                _ = shutdown.cancelled() => return Ok(()),
                _ = tokio::time::sleep(config.retry_interval) => {}
            }
        }
    }
}

struct LanePool {
    handler: Arc<dyn UserOperationHandler>,
    senders: Vec<mpsc::Sender<LaneBatchWork>>,
    tasks: Vec<JoinHandle<()>>,
}

impl LanePool {
    fn start(handler: Arc<dyn UserOperationHandler>) -> Self {
        let mut senders = Vec::with_capacity(RELAYER_LANE_COUNT as usize);
        let mut tasks = Vec::with_capacity(RELAYER_LANE_COUNT as usize);
        for lane in 0..RELAYER_LANE_COUNT {
            // The dispatcher waits for every lane before polling the next Iggy batch, so one
            // queued batch per lane is sufficient and provides natural backpressure.
            let (sender, mut receiver) = mpsc::channel::<LaneBatchWork>(1);
            let handler = handler.clone();
            tasks.push(tokio::spawn(async move {
                while let Some(work) = receiver.recv().await {
                    debug_assert!(
                        work.operations
                            .iter()
                            .all(|operation| operation.lane == lane)
                    );
                    let results = handler.handle_batch(work.operations).await;
                    let _ = work.completed.send(results);
                }
            }));
            senders.push(sender);
        }
        Self {
            handler,
            senders,
            tasks,
        }
    }

    async fn process_batch(
        &self,
        stream: &ChainStream,
        partition_id: u32,
        messages: Vec<iggy::prelude::IggyMessage>,
    ) -> BatchResult {
        let mut outcomes = messages
            .iter()
            .map(|message| (message.header.offset, false))
            .collect::<Vec<_>>();
        let mut lane_operations = (0..RELAYER_LANE_COUNT)
            .map(|_| Vec::<(usize, RoutedUserOperation)>::new())
            .collect::<Vec<_>>();
        let mut malformed = Vec::new();
        let mut fatal_lane_failure = false;

        for (index, message) in messages.into_iter().enumerate() {
            let offset = message.header.offset;
            match parse_routed_operation(stream, partition_id, offset, &message.payload) {
                Ok(operation) => {
                    lane_operations[operation.lane as usize].push((index, operation));
                }
                Err(error) => malformed.push((
                    index,
                    malformed_operation(stream, partition_id, offset, &message.payload, error),
                )),
            }
        }

        let mut completions = Vec::with_capacity(RELAYER_LANE_COUNT as usize);
        for (lane, indexed_operations) in lane_operations.into_iter().enumerate() {
            if indexed_operations.is_empty() {
                continue;
            }
            let positions = indexed_operations
                .iter()
                .map(|(index, operation)| LaneOperationPosition {
                    index: *index,
                    offset: operation.offset,
                    user_operation_hash: operation.user_operation_hash.clone(),
                })
                .collect::<Vec<_>>();
            let operations = indexed_operations
                .into_iter()
                .map(|(_, operation)| operation)
                .collect::<Vec<_>>();
            let (completed, completion) = oneshot::channel();
            if self.senders[lane]
                .send(LaneBatchWork {
                    operations,
                    completed,
                })
                .await
                .is_ok()
            {
                completions.push(LaneBatchCompletion {
                    lane: lane as u8,
                    positions,
                    completed: completion,
                });
            } else {
                fatal_lane_failure = true;
                tracing::error!(
                    chain_id = stream.chain_id,
                    stream = %stream.name,
                    partition_id,
                    lane,
                    count = positions.len(),
                    "UserOperation relayer lane stopped before accepting a batch"
                );
            }
        }

        // Lane batches are already executing concurrently while malformed records are persisted.
        // A malformed offset is acknowledged only after the handler confirms a durable
        // rejection/dead-letter write.
        for (index, operation) in malformed {
            tracing::error!(
                chain_id = stream.chain_id,
                stream = %stream.name,
                partition_id,
                offset = operation.offset,
                user_operation_hash = ?operation.user_operation_hash,
                error = %operation.error,
                "malformed Iggy UserOperation requires durable rejection or dead-lettering"
            );
            match self.handler.handle_malformed(operation).await {
                Ok(()) => outcomes[index].1 = true,
                Err(error) => tracing::error!(
                    chain_id = stream.chain_id,
                    stream = %stream.name,
                    partition_id,
                    offset = outcomes[index].0,
                    %error,
                    "malformed UserOperation was not durably rejected or dead-lettered"
                ),
            }
        }

        for completion in completions {
            let expected = completion.positions.len();
            match completion.completed.await {
                Ok(results) if results.len() == expected => {
                    for (position, result) in completion.positions.into_iter().zip(results) {
                        match result {
                            Ok(()) => outcomes[position.index].1 = true,
                            Err(error) => tracing::warn!(
                                chain_id = stream.chain_id,
                                stream = %stream.name,
                                partition_id,
                                offset = position.offset,
                                lane = completion.lane,
                                user_operation_hash = %position.user_operation_hash,
                                %error,
                                "UserOperation batch item did not reach a durable outcome"
                            ),
                        }
                    }
                }
                Ok(results) => {
                    fatal_lane_failure = true;
                    tracing::error!(
                        chain_id = stream.chain_id,
                        stream = %stream.name,
                        partition_id,
                        lane = completion.lane,
                        expected,
                        actual = results.len(),
                        "UserOperation batch handler returned a misaligned result vector"
                    );
                }
                Err(_) => {
                    fatal_lane_failure = true;
                    for position in completion.positions {
                        tracing::warn!(
                            chain_id = stream.chain_id,
                            stream = %stream.name,
                            partition_id,
                            offset = position.offset,
                            lane = completion.lane,
                            user_operation_hash = %position.user_operation_hash,
                            "UserOperation batch handler stopped before acknowledgement"
                        );
                    }
                }
            }
        }

        batch_result(&outcomes, fatal_lane_failure)
    }

    async fn shutdown(self) {
        drop(self.senders);
        for task in self.tasks {
            if let Err(error) = task.await {
                tracing::warn!(?error, "UserOperation relayer lane stopped unexpectedly");
            }
        }
    }
}

struct LaneBatchWork {
    operations: Vec<RoutedUserOperation>,
    completed: oneshot::Sender<UserOperationBatchResults>,
}

struct LaneBatchCompletion {
    lane: u8,
    positions: Vec<LaneOperationPosition>,
    completed: oneshot::Receiver<UserOperationBatchResults>,
}

struct LaneOperationPosition {
    index: usize,
    offset: u64,
    user_operation_hash: String,
}

#[derive(Debug, Eq, PartialEq)]
struct BatchResult {
    highest_contiguous_durable_offset: Option<u64>,
    had_failure: bool,
    fatal_lane_failure: bool,
}

fn batch_result(outcomes: &[(u64, bool)], fatal_lane_failure: bool) -> BatchResult {
    let highest_contiguous_durable_offset = outcomes
        .iter()
        .take_while(|(_, durable)| *durable)
        .map(|(offset, _)| *offset)
        .last();
    BatchResult {
        highest_contiguous_durable_offset,
        had_failure: outcomes.iter().any(|(_, durable)| !durable),
        fatal_lane_failure,
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueueEnvelope {
    schema_version: u32,
    user_operation_hash: String,
    chain_id: u64,
    entry_point: String,
    user_operation: Value,
    /// Absent on every envelope written before submission tiers existed, and
    /// on every client that still does not name one. A name this relay does
    /// not know fails the whole envelope into the dead-letter path rather than
    /// quietly submitting at a speed nobody chose.
    #[serde(default)]
    submission_tier: Option<vela_relay_core::gas_math::SubmissionTier>,
}

fn parse_routed_operation(
    stream: &ChainStream,
    partition_id: u32,
    offset: u64,
    payload: &[u8],
) -> Result<RoutedUserOperation, UserOperationConsumerError> {
    let envelope: QueueEnvelope = serde_json::from_slice(payload).map_err(|error| {
        UserOperationConsumerError::new(format!("invalid queue envelope: {error}"))
    })?;
    if envelope.schema_version != 1 {
        return Err(UserOperationConsumerError::new(format!(
            "unsupported queue schema version {}",
            envelope.schema_version
        )));
    }
    if envelope.chain_id != stream.chain_id {
        return Err(UserOperationConsumerError::new(format!(
            "queue chain ID {} does not match stream chain ID {}",
            envelope.chain_id, stream.chain_id
        )));
    }

    let sender = envelope
        .user_operation
        .get("sender")
        .and_then(Value::as_str)
        .ok_or_else(|| UserOperationConsumerError::new("UserOperation sender is missing"))?
        .to_owned();
    let lane = relayer_lane(&sender)?;

    Ok(RoutedUserOperation {
        schema_version: envelope.schema_version,
        user_operation_hash: envelope.user_operation_hash,
        chain_id: envelope.chain_id,
        entry_point: envelope.entry_point,
        user_operation: envelope.user_operation,
        sender,
        lane,
        stream: stream.name.clone(),
        partition_id,
        offset,
        submission_tier: envelope.submission_tier,
    })
}

fn malformed_operation(
    stream: &ChainStream,
    partition_id: u32,
    offset: u64,
    payload: &[u8],
    error: UserOperationConsumerError,
) -> MalformedUserOperation {
    let user_operation_hash = serde_json::from_slice::<Value>(payload)
        .ok()
        .and_then(|value| {
            value
                .get("userOperationHash")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    MalformedUserOperation {
        chain_id: stream.chain_id,
        stream: stream.name.clone(),
        partition_id,
        offset,
        user_operation_hash,
        payload: payload.to_vec(),
        error: error.to_string(),
    }
}

/// Matches vela-bundler routing: the sender's low 32 bits modulo the ten EOA lanes.
pub fn relayer_lane(sender: &str) -> Result<u8, UserOperationConsumerError> {
    let address = sender
        .strip_prefix("0x")
        .ok_or_else(|| UserOperationConsumerError::new("sender must start with 0x"))?;
    if address.len() != 40 || !address.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(UserOperationConsumerError::new(
            "sender must contain exactly 20 hexadecimal bytes",
        ));
    }
    let low_32_bits = u32::from_str_radix(&address[32..], 16)
        .map_err(|_| UserOperationConsumerError::new("sender suffix is not hexadecimal"))?;
    Ok((low_32_bits % u32::from(RELAYER_LANE_COUNT)) as u8)
}

fn filter_chain_streams(names: impl IntoIterator<Item = String>) -> Vec<ChainStream> {
    let mut by_chain = BTreeMap::new();
    for name in names {
        if let Some(chain_id) = chain_id_from_stream_name(&name) {
            by_chain.insert(chain_id, name);
        }
    }
    by_chain
        .into_iter()
        .map(|(chain_id, name)| ChainStream { chain_id, name })
        .collect()
}

fn chain_id_from_stream_name(name: &str) -> Option<u64> {
    let value = name.strip_prefix("chain-")?;
    let chain_id = value.parse::<u64>().ok()?;
    (name == format!("chain-{chain_id}")).then_some(chain_id)
}

#[cfg(test)]
mod tests {
    use std::{
        str::FromStr,
        sync::{Arc, Mutex},
    };

    use iggy::prelude::IggyMessage;
    use serde_json::json;

    use super::{
        BatchResult, ChainStream, LanePool, MalformedUserOperation,
        MalformedUserOperationHandlerFuture, RoutedUserOperation, UserOperationBatchResults,
        UserOperationHandler, UserOperationHandlerFuture, batch_result, chain_id_from_stream_name,
        filter_chain_streams, parse_routed_operation, relayer_lane,
    };

    #[derive(Default)]
    struct RecordingHandler {
        batches: Mutex<Vec<Vec<(u8, u64)>>>,
        malformed_offsets: Mutex<Vec<u64>>,
    }

    impl UserOperationHandler for RecordingHandler {
        fn handle_batch(
            &self,
            operations: Vec<RoutedUserOperation>,
        ) -> UserOperationHandlerFuture<'_> {
            Box::pin(async move {
                let batch = operations
                    .iter()
                    .map(|operation| (operation.lane, operation.offset))
                    .collect::<Vec<_>>();
                let results = operations
                    .into_iter()
                    .map(|_| Ok(()))
                    .collect::<UserOperationBatchResults>();
                self.batches.lock().unwrap().push(batch);
                results
            })
        }

        fn handle_malformed(
            &self,
            operation: MalformedUserOperation,
        ) -> MalformedUserOperationHandlerFuture<'_> {
            Box::pin(async move {
                self.malformed_offsets
                    .lock()
                    .unwrap()
                    .push(operation.offset);
                Ok(())
            })
        }
    }

    fn chain_stream() -> ChainStream {
        ChainStream {
            chain_id: 42_161,
            name: "chain-42161".into(),
        }
    }

    fn queue_message(offset: u64, hash: &str, sender: Option<&str>) -> IggyMessage {
        let mut user_operation = json!({ "nonce": "0x1" });
        if let Some(sender) = sender {
            user_operation["sender"] = json!(sender);
        }
        let payload = json!({
            "schemaVersion": 1,
            "userOperationHash": hash,
            "chainId": 42161,
            "entryPoint": "0xentrypoint",
            "userOperation": user_operation,
        });
        let mut message = IggyMessage::from_str(&payload.to_string()).unwrap();
        message.header.offset = offset;
        message
    }

    #[test]
    fn propagates_session_classification_from_iggy_errors() {
        use iggy::prelude::IggyError;

        use super::UserOperationConsumerError;

        assert!(
            UserOperationConsumerError::iggy("test", IggyError::Unauthenticated).is_session_error()
        );
        assert!(
            !UserOperationConsumerError::iggy("test", IggyError::InvalidCommand).is_session_error()
        );
        assert!(!UserOperationConsumerError::new("test").is_session_error());
    }

    #[test]
    fn accepts_only_canonical_chain_stream_names() {
        assert_eq!(chain_id_from_stream_name("chain-1"), Some(1));
        assert_eq!(chain_id_from_stream_name("chain-42161"), Some(42_161));
        assert_eq!(chain_id_from_stream_name("chain-0"), Some(0));
        assert_eq!(chain_id_from_stream_name("chain-01"), None);
        assert_eq!(chain_id_from_stream_name("chain-"), None);
        assert_eq!(chain_id_from_stream_name("chain--1"), None);
        assert_eq!(chain_id_from_stream_name("chain-1-extra"), None);
        assert_eq!(chain_id_from_stream_name("my-stream"), None);
        assert_eq!(
            chain_id_from_stream_name("chain-18446744073709551616"),
            None
        );
    }

    #[test]
    fn filters_and_sorts_discovered_chain_streams() {
        let streams = filter_chain_streams(
            ["chain-42161", "my-stream", "chain-1", "chain-01"]
                .into_iter()
                .map(str::to_owned),
        );

        assert_eq!(
            streams,
            vec![
                ChainStream {
                    chain_id: 1,
                    name: "chain-1".into(),
                },
                ChainStream {
                    chain_id: 42_161,
                    name: "chain-42161".into(),
                },
            ]
        );
    }

    #[test]
    fn routes_by_sender_low_32_bits() {
        assert_eq!(
            relayer_lane("0x0000000000000000000000000000000000000000").unwrap(),
            0
        );
        assert_eq!(
            relayer_lane("0x000000000000000000000000000000000000000b").unwrap(),
            1
        );
        assert_eq!(
            relayer_lane("0x00000000000000000000000000000000FFFFFFFF").unwrap(),
            5
        );
        assert!(relayer_lane("0x1234").is_err());
    }

    #[test]
    fn parses_and_routes_a_queue_envelope() {
        let payload = serde_json::to_vec(&json!({
            "schemaVersion": 1,
            "userOperationHash": "0xhash",
            "chainId": 42161,
            "entryPoint": "0xentrypoint",
            "userOperation": {
                "sender": "0x0000000000000000000000000000000000000015",
                "nonce": "0x1"
            }
        }))
        .unwrap();
        let stream = ChainStream {
            chain_id: 42_161,
            name: "chain-42161".into(),
        };

        let operation = parse_routed_operation(&stream, 0, 12, &payload).unwrap();

        assert_eq!(operation.chain_id, 42_161);
        assert_eq!(operation.lane, 1);
        assert_eq!(operation.partition_id, 0);
        assert_eq!(operation.offset, 12);
        assert_eq!(operation.user_operation_hash, "0xhash");
        // Every envelope already in the queue was written without a tier; it
        // must keep routing, and must read as "named nothing".
        assert_eq!(operation.submission_tier, None);
    }

    #[test]
    fn carries_a_named_submission_speed_from_the_queue_envelope() {
        let envelope = |tier: &str| {
            serde_json::to_vec(&json!({
                "schemaVersion": 1,
                "userOperationHash": "0xhash",
                "chainId": 42161,
                "entryPoint": "0xentrypoint",
                "userOperation": {
                    "sender": "0x0000000000000000000000000000000000000015",
                    "nonce": "0x1"
                },
                "submissionTier": tier
            }))
            .unwrap()
        };
        let stream = ChainStream {
            chain_id: 42_161,
            name: "chain-42161".into(),
        };

        let operation = parse_routed_operation(&stream, 0, 12, &envelope("fast")).unwrap();
        assert_eq!(
            operation.submission_tier,
            Some(vela_relay_core::gas_math::SubmissionTier::Fast)
        );
        // A speed this relay cannot price fails the whole envelope into the
        // dead-letter path rather than submitting at one nobody chose.
        assert!(parse_routed_operation(&stream, 0, 12, &envelope("turbo")).is_err());
    }

    #[test]
    fn rejects_an_envelope_from_another_chain() {
        let payload = serde_json::to_vec(&json!({
            "schemaVersion": 1,
            "userOperationHash": "0xhash",
            "chainId": 1,
            "entryPoint": "0xentrypoint",
            "userOperation": {
                "sender": "0x0000000000000000000000000000000000000001"
            }
        }))
        .unwrap();
        let stream = ChainStream {
            chain_id: 42_161,
            name: "chain-42161".into(),
        };

        assert!(parse_routed_operation(&stream, 0, 12, &payload).is_err());
    }

    #[test]
    fn advances_only_through_the_contiguous_durable_prefix() {
        assert_eq!(
            batch_result(&[(10, true), (11, true), (12, false), (13, true)], false),
            BatchResult {
                highest_contiguous_durable_offset: Some(11),
                had_failure: true,
                fatal_lane_failure: false,
            }
        );
        assert_eq!(
            batch_result(&[(10, false), (11, true)], false),
            BatchResult {
                highest_contiguous_durable_offset: None,
                had_failure: true,
                fatal_lane_failure: false,
            }
        );
        assert_eq!(
            batch_result(&[(10, true), (11, true)], false),
            BatchResult {
                highest_contiguous_durable_offset: Some(11),
                had_failure: false,
                fatal_lane_failure: false,
            }
        );
        assert!(batch_result(&[(10, true)], true).fatal_lane_failure);
    }

    #[tokio::test]
    async fn sends_one_ordered_batch_to_each_lane() {
        let handler = Arc::new(RecordingHandler::default());
        let lanes = LanePool::start(handler.clone());
        let result = lanes
            .process_batch(
                &chain_stream(),
                0,
                vec![
                    queue_message(
                        10,
                        "0xhash-10",
                        Some("0x0000000000000000000000000000000000000001"),
                    ),
                    queue_message(
                        11,
                        "0xhash-11",
                        Some("0x000000000000000000000000000000000000000b"),
                    ),
                    queue_message(
                        12,
                        "0xhash-12",
                        Some("0x0000000000000000000000000000000000000002"),
                    ),
                ],
            )
            .await;
        lanes.shutdown().await;

        let mut batches = handler.batches.lock().unwrap().clone();
        batches.sort_by_key(|batch| batch[0].0);
        assert_eq!(batches, vec![vec![(1, 10), (1, 11)], vec![(2, 12)]]);
        assert_eq!(
            result,
            BatchResult {
                highest_contiguous_durable_offset: Some(12),
                had_failure: false,
                fatal_lane_failure: false,
            }
        );
    }

    #[tokio::test]
    async fn durably_dead_letters_malformed_messages_without_blocking_the_partition() {
        let handler = Arc::new(RecordingHandler::default());
        let lanes = LanePool::start(handler.clone());
        let result = lanes
            .process_batch(
                &chain_stream(),
                0,
                vec![
                    queue_message(20, "0xmalformed", None),
                    queue_message(
                        21,
                        "0xvalid",
                        Some("0x0000000000000000000000000000000000000002"),
                    ),
                ],
            )
            .await;
        lanes.shutdown().await;

        assert_eq!(*handler.malformed_offsets.lock().unwrap(), vec![20]);
        assert_eq!(
            result,
            BatchResult {
                highest_contiguous_durable_offset: Some(21),
                had_failure: false,
                fatal_lane_failure: false,
            }
        );
    }
}
