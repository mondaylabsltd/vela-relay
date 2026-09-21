//! Business vocabulary and decision rules for the Vela ERC-4337 relay.
//!
//! This crate owns every business decision the service makes and is
//! deliberately I/O-free: no Redis, no Iggy, no HTTP, no clocks. The server
//! crate (`vela-relay`) is the shell that executes the decisions made here
//! against real infrastructure and reports results back as data.
//!
//! Modules are business domains, not architectural layers:
//!
//! - [`task`] — shared vocabulary: the user-operation record, its status
//!   enumeration, and the queue envelope.
//! - [`admission`] — the two-phase eth_sendUserOperation program: validation,
//!   durable record, queue append, admitted mark.
//! - [`lifecycle`] — the single authoritative status transition table and the
//!   patch/bundle-submission decisions every durable status write flows
//!   through.
//! - [`vault`] — deterministic HKDF key derivation for the treasury and the
//!   relayer pool, and sender→lane routing.
//! - [`gas_math`] — EIP-1559 price arithmetic: fee-history interpretation,
//!   tier scaling, quantity parsing.
//! - [`hold`] — the delayed-inbox retry ladder and the settlement hold budget.
//! - [`settlement`] — in-band settlement: reimbursement parsing, evaluation,
//!   repricing, and the accept/reprice/keep verdict.
//! - [`cost`] — deterministic bundle gas allocation and native cost.
//! - [`abi`] — ERC-4337 packing, userOpHash, handleOps/getNonce calldata.
//! - [`receipt`] — receipt status and UserOperationEvent extraction.
//! - [`signing`] — deterministic EIP-1559 / Tempo `0x76` transaction signing
//!   (key bytes are injected per call; custody stays in the shell).
//! - [`broadcast`] — raw-transaction validation and the judgement of
//!   ambiguous/rejected broadcasts.
//! - [`funding`] — relayer float targets, top-up caps, and treasury
//!   affordability.
//! - [`tempo`] — Tempo chain constants and pathUSD calldata builders.
//! - [`alchemy`] — the static Alchemy network registry.
//!
//! Nondeterministic inputs (wall-clock time, generated identifiers, chain
//! context, market prices, policy values) always enter through an event or an
//! operation result supplied by the shell; nothing in this crate observes a
//! clock, randomness, or the environment.

pub mod abi;
pub mod account;
pub mod admission;
pub mod alchemy;
pub mod alert;
pub mod broadcast;
pub mod cost;
pub mod estimate;
pub mod execution;
pub mod funding;
pub mod gas_math;
pub mod hold;
pub mod lifecycle;
pub mod quote;
pub mod receipt;
pub mod settlement;
pub mod signing;
pub mod simulation;
pub mod task;
pub mod tempo;
pub mod treasury;
pub mod vault;
pub mod wire;
