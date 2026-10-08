# Which RPC the relay uses

The relay reads every chain through someone's JSON-RPC endpoint, and signs and
sends on it through someone's endpoint. This page says whose, in what order, which
hosts it refuses, and what the treasury probe answers when it has none. Every rule
here is the same in the docker and the Cloudflare deployments; the decisions live
in `vela-relay-core` (`rpc_host`, `chain_directory::Listing`,
`treasury::unreadable`).

## Reads: the wallet's RPC first

Everything a request reads — the treasury probe, the in-band quote, gas prices,
gas estimates, the account view, a token's decimals at admission — tries, in order:

1. the URL in the request's `x-vela-rpc-url` header;
2. Alchemy, when `ALCHEMY_API_KEY` is set and Alchemy serves the chain;
3. the chain directory's endpoints for the chain, in published order.

Since vela-wallet spec 098 (October 2026) every Vela app sends, on every request
to the relay, the RPC address it uses for that chain: the one the person set, the
one built from a provider key they added, or the built-in one — **including any API
key in it**. The apps say so where the person sets an address or a key, and the
privacy policy says so. It is what lets a relay — Vela's or anyone's — read the
network the person actually uses.

The relay does not write that key anywhere: logs carry only `scheme://host:port/…`,
and the `x-vela-rpc-domain` response header only the host.

## Broadcasting: never the wallet's RPC

The executor signs with the relay's own keys and sends from a lane that runs after
the request has returned. It uses, in order:

1. `VELA_RELAY_EXECUTOR_RPC_URLS` — the operator's own endpoints for a chain;
2. Alchemy;
3. the chain directory.

Never `x-vela-rpc-url`. A caller-supplied endpoint could lie about a nonce, a
receipt or a balance and make the relay send again, spending the float that every
person on that chain depends on.

### A method the chain's nodes do not have

The executor simulates every operation before it signs, in three tiers:
`eth_simulateV1`, then the Pimlico simulation contracts through `eth_call`, then
`debug_traceCall`. Each tier walks the endpoint list above, one endpoint at a
time, until one answers.

Avalanche's C-Chain client does not implement `eth_simulateV1`. On 2026-10-08
every one of the 28 endpoints the directory lists for 43114 answered `-32601`
("method does not exist") or not at all, so each simulation walked the whole list
before falling back, twice a pass, and an AVAX send took about a minute
(vela-wallet #464). So:

- **Avalanche (43114) and Fuji (43113)** ask for `eth_simulateV1` last.
- **Any chain** where a whole walk got only "no such method" answers (JSON-RPC
  `-32601`, or an error that says the method is missing) and no `result` is
  treated the same way for 10 minutes. Endpoints that timed out or answered with
  an HTTP error are no evidence either way. One endpoint that serves the method
  ends it at once.

"Last" means after the other two tiers, and only for the operations they could
not decide. The method is never skipped, so a wrong belief costs one slow walk,
never a verdict. The rule is `vela-relay-core`'s `simulation::simulate_v1_turn`
and `rpc_walk`. Each process (docker) or isolate (Workers) keeps its own memory,
which starts empty.

**Considered and not built** (2026-10-03): broadcasting through the wallet's RPC
for a chain whose directory entry has no usable endpoint. Measured on a snapshot of
the chain list the directory serves (ethereum-lists, 2026-05-06): of 2,602 chains,
198 have no public `https` endpoint, nearly all of them deprecated testnets or
local development ids such as 1337. A chain the directory does not list cannot be
served either way — its quote and its executor both need the chain's native asset
from the directory. And a private chain is served by a relay run beside it (below).
That was too little to justify putting a stranger's URL in the money path.

## Which hosts: public `https` only

Any URL the relay did not choose itself — the wallet's header, and every endpoint
the directory lists — is used only if it is `https` to a public host. Refused:

- `localhost` and `*.localhost`;
- loopback, unspecified, and private addresses (`127.0.0.0/8`, `0.0.0.0`,
  `10/8`, `172.16/12`, `192.168/16`, IPv6 `::1`, `::`, `fc00::/7`);
- link-local addresses, which hold the cloud metadata service
  (`169.254.169.254`, `fe80::/10`);
- carrier-grade shared space (`100.64/10`);
- the IPv4-mapped IPv6 form of any of these.

Otherwise anyone on the internet could make the relay call into its own network —
the metadata service, the Redis beside it. The rule judges the host as written; a
public name that resolves to a private address is not caught.

`VELA_RELAY_EXECUTOR_RPC_URLS` is not subject to it: those endpoints are the
operator's own choice, and may be `http`.

### A relay beside a private chain

A node on your machine or your LAN — Anvil, Hardhat, a private network — can only
be served by a relay that can reach it. On a relay you run for yourself:

```dotenv
# Allow http and private hosts in the wallet's header and the directory's list.
VELA_RELAY_ALLOW_PRIVATE_RPC=true
# The executor's endpoint for the chain, tried before anything else.
VELA_RELAY_EXECUTOR_RPC_URLS={"31337":"http://127.0.0.1:8545"}
```

Set the executor's endpoint even when the directory lists the chain: Vela's
directory gives 31337 to GoChain Testnet, not to your Anvil node. If the directory
does not list the chain at all, list it in your own directory
(`VELA_RELAY_CHAIN_DIRECTORY_URL`, see the README) — the quote and the executor
need its native asset.

Never set `VELA_RELAY_ALLOW_PRIVATE_RPC` on a relay that strangers can reach: it
lets any request make the relay fetch any address on your network. Vela's own
deployment never sets it. On Cloudflare Workers it has little use, because a Worker
cannot reach a private network anyway.

## The treasury probe: "cannot" or "not now"

`GET /v1/treasury/{chainId}` is what the wallet asks before it lets anyone sign. It
must not answer "not now" for a chain the relay can never serve: the wallet treats
`503` as transient and carries on, so the person signed and the send then failed.
Until October 2026 that is what happened for every chain the relay could not reach
— live, chains 1337, 31337 and 123456789 all answered `503`.

| Case | Status | Body |
|---|---|---|
| The balance was read | `200` | `{chainId, address, asset, balance, floor, bootstrapNeeded}` |
| The directory does not list the chain — a `404`, or Vela's directory's HTML page for an unknown chain | `404` | `{"error": "this relay's chain directory does not list the chain", "reason": "not_listed"}` |
| There is no endpoint the relay may use: no usable header, no Alchemy, and none in the directory's entry (1337 lists only `http://127.0.0.1:8545`) | `404` | `{"error": "this relay has no RPC it can use for the chain", "reason": "no_rpc"}` |
| The wallet's RPC was refused (private or `http`) and nothing else answered — the network the person uses is out of reach (31337: their Anvil node; the directory's GoChain endpoint is dead) | `404` | `reason: "no_rpc"`, as above |
| A usable endpoint did not answer, the directory itself did not answer, or the answer was not a balance | `503` | `{"error": "…"}` |

The directory is asked first, even when the wallet's RPC would answer: a chain it
does not list cannot be quoted or executed, so a balance for it would only send the
person on to a send that fails after signing. A directory that did not answer says
nothing about the chain, so it is never a `404`.

On a `404` the wallet stops the send at **Continue** and says the relay can't reach
the network — the operator's to fix on a network Vela ships, a public `https` RPC
or a relay of your own on one the person added. On `bootstrapNeeded: true` it shows
the treasury's address and how much it needs, and carries on by itself once funded.
The wallet reads only the status; `reason` is for whoever reads the response.
