# Migration Guide

This guide covers the breaking changes between Router versions from the perspective of users who consume the Rust
encoding library or interact with the TychoRouterV3 contracts. Migrating from V2 means working through both sections in
order.

## V2 to V3

{% hint style="info" %}
To keep using Router V2, please encode your swap with `tycho-execution<=0.165.1` . All higher versions support only
Router V3.
{% endhint %}

### Encoding Changes

#### Solution Struct

**Renamed fields:**

<table><thead><tr><th width="210">V2</th><th width="210">V3</th><th width="280">Notes</th></tr></thead><tbody><tr><td><code>given_token</code></td><td><code>token_in</code></td><td>The input token</td></tr><tr><td><code>given_amount</code></td><td><code>amount_in</code></td><td>Amount of the input token</td></tr><tr><td><code>checked_token</code></td><td><code>token_out</code></td><td>The output token</td></tr><tr><td><code>checked_amount</code></td><td><code>min_amount_out</code></td><td>Minimum acceptable output amount</td></tr></tbody></table>

**Removed fields:**

<table><thead><tr><th width="280">Field</th><th width="420">Replacement</th></tr></thead><tbody><tr><td><code>native_action: Option&#x3C;NativeAction></code></td><td>The encoder now inserts WETH wrap/unwrap swaps automatically (see <a href="encoding/#native-tokens">Native Tokens</a>).</td></tr><tr><td><code>exact_out: bool</code></td><td>Only exact-in was ever supported. Removed for simplicity.</td></tr></tbody></table>

**New fields:**

<table><thead><tr><th width="210">Field</th><th width="210">Type</th><th width="280">Description</th></tr></thead><tbody><tr><td><code>user_transfer_type</code></td><td><code>UserTransferType</code></td><td>How user funds enter the router. Moved here from the encoder builder.</td></tr></tbody></table>

**Private fields with getters/setters:**

`Solution` fields are now private — use the constructor and builder methods instead of direct field access:

```rust
// V2
let solution = Solution {
sender: addr,
receiver: addr,
given_token: token_a,
given_amount: amount,
checked_token: token_b,
checked_amount: min_out,
swaps: vec![swap],
exact_out: false,
native_action: Some(NativeAction::Wrap),
};

// V3
let solution = Solution::new(
addr,        // sender
addr,        // receiver
token_a,     // token_in
token_b,     // token_out
amount,      // amount_in
min_out,     // min_amount_out
vec![swap],  // swaps
)
.with_user_transfer_type(UserTransferType::TransferFrom);
```

#### UserTransferType Moved to Solution

`UserTransferType` has moved from the encoder builder to each `Solution`, so solutions in the same batch can use different funding methods.

```rust
// V2
let encoder = TychoRouterEncoderBuilder::new()
.chain(chain)
.user_transfer_type(UserTransferType::TransferFrom)  // set here
.swap_encoder_registry(registry)
.build() ?;

// V3
let encoder = TychoRouterEncoderBuilder::new()
.chain(chain)
.swap_encoder_registry(registry)
.build() ?;

let solution = Solution::new(/* ... */)
.with_user_transfer_type(UserTransferType::TransferFrom);  // set here
```

The `UserTransferType::None` variant has been renamed to `UserTransferType::UseVaultsFunds`, reflecting the new
vault-based architecture.

#### Swap Struct

**Builder methods renamed** (added `with_` prefix for consistency):

| V2                             | V3                                  |
|--------------------------------|-------------------------------------|
| `.split(0.5)`                  | `.with_split(0.5)`                  |
| `.user_data(data)`             | `.with_user_data(data)`             |
| `.protocol_state(state)`       | `.with_protocol_state(state)`       |
| `.estimated_amount_in(amount)` | `.with_estimated_amount_in(amount)` |

**Getter methods renamed** (dropped `get_` prefix):

| V2                           | V3                       |
|------------------------------|--------------------------|
| `.get_split()`               | `.split()`               |
| `.get_user_data()`           | `.user_data()`           |
| `.get_protocol_state()`      | `.protocol_state()`      |
| `.get_estimated_amount_in()` | `.estimated_amount_in()` |

**`token_in` / `token_out` are now `Token`, not `Bytes`:**

In V2 these fields were `Bytes` (raw addresses). In V3 they are `tycho_common::models::token::Token`, carrying decimals,
symbol, and tax/gas metadata alongside the address. Wrap a raw address with the `default_token(addr)` test helper
(available under `#[cfg(any(test, feature = "test-utils"))]`) when full token metadata isn't needed.

```rust
// V2
let swap = Swap::new(component, token_in_bytes, token_out_bytes);

// V3
let swap = Swap::new(component, token_in_token, token_out_token, estimated_gas);
```

**New required parameter on `Swap::new`:**

The constructor now takes a per-swap simulation gas estimate as its 4th argument. The new field is exposed
via `.estimated_gas() -> &BigUint`.

#### EncodedSolution Struct

Fields are now private with getter methods, matching the pattern used elsewhere:

```rust
// V2
let swaps = encoded_solution.swaps;
let sig = encoded_solution.function_signature;

// V3
let swaps = encoded_solution.swaps();
let sig = encoded_solution.function_signature();
```

The `function_signature` field now reflects both the swap strategy and the funding mode. For
example, `splitSwapUsingVault(...)` for a split swap using vault funds.

**Removed `permit` field:**

The `permit: Option<PermitSingle>` field has been removed from `EncodedSolution`. The encoder no longer creates or
returns Permit2 data. If you use `TransferFromPermit2`, you must handle permit creation and signing yourself.

The `Permit2` utility struct has been made public, so you can use it directly.

**New `estimated_gas` field:**

`EncodedSolution` now exposes a `estimated_gas: BigUint` (via `.estimated_gas()`), derived from each
swap's `estimated_gas` and some overheads (from the router and token transfers). Users can use this as minimum estimated
gas for this solution.

#### Wrapping and Unwrapping

V2 used a `NativeAction` enum on the `Solution` with `Wrap` and `Unwrap` variants. The router had dedicated wrap/unwrap
flags.

**V3 removes this entirely.** Instead, a WETH executor handles wrapping and unwrapping as regular swap steps. The
encoder automatically inserts these swaps when it detects ETH↔WETH gaps in the swap path.

```rust
// V2
let solution = Solution {
given_token: eth_address,
checked_token: dai_address,
native_action: Some(NativeAction::Wrap),
swaps: vec![weth_to_dai_swap],
..
};

// V3 — just set token_in to ETH; the encoder adds a WETH wrap swap automatically
let solution = Solution::new(
sender,
receiver,
eth_address,   // token_in is ETH
dai_address,   // token_out is DAI
amount,
min_out,
vec![weth_to_dai_swap],  // first swap expects WETH — encoder bridges the gap
);
```

This also works for mid-path bridging (e.g., if one swap outputs ETH and the next expects WETH) and at the end of a
path. See more in [Native Tokens](encoding/#native-tokens).

#### Encoder Builder

**Removed options:**

| V2 option                  | Notes                             |
|----------------------------|-----------------------------------|
| `.user_transfer_type(...)` | Moved to `Solution`.              |
| `.swapper_pk(...)`         | Removed. Sign Permit2 externally. |
| `.historical_trade()`      | Removed. No longer needed.        |

The V3 builder only requires `chain` and `swap_encoder_registry`:

```rust
// V3
let encoder = TychoRouterEncoderBuilder::new()
.chain(Chain::Ethereum)
.swap_encoder_registry(registry)
.build() ?;
```

#### Transaction and encode\_full\_calldata Removed

The `Transaction` struct and `encode_full_calldata` method have been removed entirely. In V2, `encode_full_calldata` was
already deprecated. V3 only supports `encode_solutions`, which returns `EncodedSolution` objects.

You are responsible for constructing the full method call, including execution-critical parameters
like `min_amount_out`, `receiver`, and fee configuration.

#### SwapEncoderRegistry

`SwapEncoderRegistry::new` now requires a `Chain` parameter:

```rust
// V2
let registry = SwapEncoderRegistry::new()
.add_default_encoders(executors_addresses)?;

// V3
let registry = SwapEncoderRegistry::new_with_defaults(Chain::Ethereum)?;
```

### Execution Changes

#### Router Function Signatures

The TychoRouterV3 methods now include a `ClientFeeParams` struct in their signatures:

```solidity
struct ClientFeeParams {
    uint16 clientFeeBps;
    address clientFeeReceiver;
    uint256 maxClientContribution;
    uint256 deadline;
    bytes clientSignature;
}
```

When constructing calldata yourself (recommended), encode this struct as part of the function arguments. Even if you are
not charging fees, you must pass this parameter with zero values.

A `ClientFeeParams` Rust struct matching this Solidity struct is available in `tycho-execution`. Clients are
responsible for constructing and signing it — the encoder does not use it internally. Call `.into_abi_params()` to
convert it to the ABI-encodable tuple:

```rust
// No fee (zero values)
let client_fee_params = ClientFeeParams::default().into_abi_params();

// With a fee
let client_fee_params = ClientFeeParams {
    client_fee_bps: 50,
    client_fee_receiver: fee_receiver_bytes,
    ..ClientFeeParams::default()
}.into_abi_params();
```

#### Vault Integration

The TychoRouterV3 now includes an ERC6909 vault. Key changes:

* **`UseVaultsFunds`** replaces the old `None` transfer type. Tokens deposited in the vault are tracked per-user and can
  be used for swaps or withdrawn.
* Deposit tokens via `router.deposit(token, amount)` before swapping with vault funds.
* Fees (both client and router fees) are credited to the receiver's vault balance rather than transferred immediately.

For more see [Vault](vault.md).

#### No More Wrap/Unwrap Flags

The router no longer accepts `wrap` or `unwrap` boolean flags. If your calldata construction includes these parameters,
remove them. The WETH executor handles wrapping and unwrapping as part of the swap path.
See [Native Tokens](encoding/#native-tokens "mention").

#### Native ETH Address

When constructing the outer function arguments (`tokenIn` / `tokenOut`), native ETH must be represented as `0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE` — not `address(0)`. The router reverts on `address(0)`.

The `ROUTER_ETH_ADDRESS` constant is exported from the `tycho-execution` crate for this purpose.

#### Method Variants

Each swap strategy (single, sequential, split) gains a third variant — `UsingVault` — alongside the existing standard and Permit2 variants:

| V2                       | V3                          |
|--------------------------|-----------------------------|
| `singleSwap(...)`        | `singleSwap(...)`           |
| `singleSwapPermit2(...)` | `singleSwapPermit2(...)`    |
| —                        | `singleSwapUsingVault(...)` |

`sequentialSwap` and `splitSwap` follow the same pattern. Use `EncodedSolution.function_signature` to determine which variant to call.

## V3 to V3.1

Two changes drive the V3.1 migration:

1. The router now takes **both** a quoted output amount and a minimum output amount, and bounds the
   minimum against the quote.
2. The client fee rate moved from basis points to the 8-decimal fee unit the router's own fees already
   used. That widens the field, changes the client fee typehash, and invalidates every V3.0 signature.

{% hint style="info" %}
Router V3.0 stays available on the `tycho-execution` releases that precede this change. Pin your
dependency to the last of those releases to keep using it. All later versions target V3.1.
{% endhint %}

### Encoding Changes

#### Solution: min\_amount\_out replaced by amount\_out and slippage

`Solution::min_amount_out` is gone. Two fields replace it:

<table>
<thead><tr><th width="180">Field</th><th width="120">Type</th><th>Description</th></tr></thead>
<tbody>
<tr><td><code>amount_out</code></td><td><code>BigUint</code></td><td>The output amount your simulation quoted. Becomes the router's <code>expectedAmountOut</code></td></tr>
<tr><td><code>slippage</code></td><td><code>f64</code></td><td>Maximum negative slippage you accept, as a fraction (<code>0.0025</code> = 0.25%)</td></tr>
</tbody>
</table>

`Solution::new` takes both, in that order, growing from 7 arguments to 8:

```rust
// V3.0
let solution = Solution::new(
    sender,
    receiver,
    token_in,
    token_out,
    amount_in,
    min_amount_out,   // computed off-chain from your slippage tolerance
    vec![swap],
);

// V3.1
let solution = Solution::new(
    sender,
    receiver,
    token_in,
    token_out,
    amount_in,
    simulated_amount_out, // amount_out
    0.0025,               // slippage — 0.25%
    vec![swap],
);
```

Accessors and builder methods follow:

| V3.0 | V3.1 |
|------|------|
| `.min_amount_out() -> &BigUint` — stored field | `.min_amount_out() -> BigUint` — derived from `amount_out` and `slippage` |
| — | `.amount_out()` and `.slippage()` for the raw fields |
| `.with_min_amount_out(amount)` | `.with_amount_out(amount)` and `.with_slippage(fraction)` |

`min_amount_out()` keeps its name but now returns by value, computing `amount_out * (1 - slippage)` at
basis-point granularity, rounded down. Read sites need a `&` where they previously passed the borrowed
getter result:

```rust
let amount_out = biguint_to_u256(solution.amount_out());          // -> expectedAmountOut
let min_amount_out = biguint_to_u256(&solution.min_amount_out()); // -> minAmountOut
```

Two edge cases the derivation introduces: a `slippage` below half a basis point rounds away to zero
tolerance, and a `slippage` outside `0.0..=1.0` clamps into range rather than panicking.

The `tycho-encode` CLI's JSON input changes the same way: replace `min_amount_out` with `amount_out`
and `slippage`.

#### Solution validation rejects a zero amount\_out

`validate_solution` now fails when `amount_out` is zero, because the router rejects a zero
`expectedAmountOut`. Encoding a solution with a placeholder amount — a pattern that worked when the
field only fed `minAmountOut` — returns a `FatalError`.

#### ClientFeeParams: client\_fee\_bps widened to u32

`ClientFeeParams::client_fee_bps` changes from `u16` to `u32`, and its unit changes from basis points
to the 8-decimal fee unit described in [Fee units](#fee-units). `ClientFeeParams::new` and
`into_abi_params` follow suit.

```rust
// V3.0 — 1 BPS in basis points
let params = ClientFeeParams::new(receiver, signature, deadline, 1u16);

// V3.1 — 1 BPS in fee units
let params = ClientFeeParams::new(receiver, signature, deadline, 10_000u32);
```

Multiply your existing basis-point rates by `10_000`.

### Execution Changes

#### Router methods take expectedAmountOut

Every swap method gains an `expectedAmountOut` parameter directly before `minAmountOut`. All nine
methods change, so all nine selectors change.

```solidity
// V3.0
function singleSwap(
    uint256 amountIn,
    address tokenIn,
    address tokenOut,
    uint256 minAmountOut,
    address receiver,
    ClientFeeParams calldata clientFeeParams,
    bytes calldata swapData
) public payable returns (uint256);

// V3.1
function singleSwap(
    uint256 amountIn,
    address tokenIn,
    address tokenOut,
    uint256 expectedAmountOut,
    uint256 minAmountOut,
    address receiver,
    ClientFeeParams calldata clientFeeParams,
    bytes calldata swapData
) public payable returns (uint256);
```

`sequentialSwap`, `splitSwap`, their `Permit2` variants, and their `UsingVault` variants take the new
parameter in the same position. `splitSwap` keeps `nTokens` between `minAmountOut` and `receiver`.

#### The router bounds minAmountOut against expectedAmountOut

V3.0 accepted any non-zero `minAmountOut`, including `1`. V3.1 requires it to sit inside a window
anchored on `expectedAmountOut`:

```
expectedAmountOut * (10_000 - MAX_SLIPPAGE_TOLERANCE_BPS) / 10_000  <=  minAmountOut  <=  expectedAmountOut
```

`MAX_SLIPPAGE_TOLERANCE_BPS` is `2_000`, putting the floor 20% below the quote. Values outside that
window — including zero — revert with `TychoRouter__InvalidMinAmountOut`.

Two consequences for existing integrations:

* Calldata that passed `minAmountOut = 1` (or any near-zero floor) now reverts. Compute a real floor
  from your slippage tolerance.
* `expectedAmountOut` sets both ends of the window, so raising it also raises the lower bound. Pass
  the amount your simulation returned.

The router also rejects a zero `expectedAmountOut` with `TychoRouter__AmountOutZero`.

#### Fee units <a href="#fee-units" id="fee-units"></a>

The router's own fee rates already used an 8-decimal fee unit in V3.0. V3.1 puts `clientFeeBps` on the
same scale and widens it from `uint16` to `uint32`, which lets clients charge sub-BPS rates:

| Rate | Fee units |
|------|-----------|
| 100% | `100_000_000` |
| 1% | `1_000_000` |
| 1 BPS (0.01%) | `10_000` |
| 0.1 BPS (0.001%) | `1_000` |

Existing basis-point client rates convert by multiplying by `10_000`. A rate you pass unconverted
charges 10,000 times less than you intend, not more.

The FeeCalculator's public constants also change name:

| V3.0 | V3.1 |
|------|------|
| `MAX_FEE_BPS` | `MAX_BPS` |
| `MAX_FEE_BPS_SQUARED` | `MAX_BPS_SQUARED` |

Their values are unchanged: `100_000_000` and `MAX_BPS²` — the combined denominator when the router
charges a fee on another fee.

#### The client fee typehash changes

The `ClientFee` typehash gains `expectedAmountOut` and widens `clientFeeBps`, so every V3.0 signature
fails verification against V3.1:

```solidity
// V3.0
ClientFee(uint16 clientFeeBps, address clientFeeReceiver, uint256 maxClientContribution,
          uint256 deadline, uint256 amountIn, address tokenIn, address tokenOut,
          uint256 minAmountOut, address receiver, bytes swaps)

// V3.1
ClientFee(uint32 clientFeeBps, address clientFeeReceiver, uint256 maxClientContribution,
          uint256 deadline, uint256 amountIn, address tokenIn, address tokenOut,
          uint256 expectedAmountOut, uint256 minAmountOut, address receiver, bytes swaps)
```

`expectedAmountOut` sits directly before `minAmountOut`, matching the router argument order. Hash
`swaps` — the encoded swap graph you pass to the router — with `keccak256` in the struct hash, as
EIP-712 requires for dynamic types.

As in V3.0, the signature binds the whole swap, so **encode first, then sign**: a signature only
validates for a swap with identical input parameters. `clientFeeReceiver` must be an EOA — the router
uses `ECDSA.recover` with no ERC-1271 fallback.

{% hint style="warning" %}
Published V3.0 documentation described a four-field `ClientFee` struct covering only the fee
parameters. That was never accurate — V3.0 already bound the swap intent. If you implemented signing
from that description, your signatures do not verify against V3.0 either.
{% endhint %}

The EIP-712 domain is unchanged: `name = "TychoRouter"`, `version = "1"`, `verifyingContract` set to
the router address. Read the router's `CLIENT_FEE_TYPEHASH` to confirm the typehash you sign matches.

See [Client Fee Signature](encoding/#client-fee-signature) for a full signing example.

#### Positive slippage and the fee base

V3.0 calculated fees from the amount the swap produced. V3.1 keeps that behaviour and adds one wrinkle:
when the router captures positive slippage, it charges fees on the output **net of** that surplus.

The router may capture output above `expectedAmountOut`, so it does not guarantee that surplus beyond
your quote reaches the receiver — amounts between `minAmountOut` and `expectedAmountOut` always do.
Query `getPositiveSlippageEnabled()` on the FeeCalculator to check whether capture is active.

The router reverts with `TychoRouter__FeesExceedOutput` if the calculated fees exceed the swap output.

#### FeeCalculator interface

Anyone reading fee configuration on-chain needs to update these calls:

| V3.0 | V3.1 |
|------|------|
| `calculateFee(amountIn, client, clientFeeBps)` → `(amountOut, FeeRecipient[])` | `calculateFee(FeeInput)` → `FeeRecipient[]` — always two entries, `[router, client]` |
| `getEffectiveRouterFeeOnOutput(client)` | Removed |
| `getEffectiveRouterFeeOnOutputScaled(client)` | Removed |
| `getEffectiveRouterFeeOnClientFee(client)` | Removed |
| — | `mustOutputThroughRouter(clientFeeBps, client)` → `bool` — whether output must pass through the router before reaching the receiver |
| — | `getPositiveSlippageEnabled()` → `bool` — whether the router captures output above `expectedAmountOut` |

`getAllClientFees(start, count)` still returns per-client overrides, so read effective rates from there
instead of the removed getters.

`FeeInput` bundles the swap context the calculator needs:

```solidity
struct FeeInput {
    uint256 actualAmountOut;
    uint256 expectedAmountOut;
    uint256 amountIn;
    address tokenIn;
    address tokenOut;
    uint32 clientFeeBps;
    address client;
}
```

`calculateFee` no longer returns the post-fee amount — subtract the recipients' amounts from the swap
output yourself.

#### Revert reasons

New errors:

| Error | Cause |
|-------|-------|
| `TychoRouter__InvalidMinAmountOut(minAmountOut, expectedAmountOut)` | `minAmountOut` is zero, above `expectedAmountOut`, or more than `MAX_SLIPPAGE_TOLERANCE_BPS` below it |
| `TychoRouter__AmountOutZero()` | `expectedAmountOut` is zero |
| `TychoRouter__FeesExceedOutput(totalFees, actualAmountOut)` | Calculated fees exceed the swap output |

Removed error:

| Error | Replacement |
|-------|-------------|
| `TychoRouter__UndefinedMinAmountOut()` | `TychoRouter__InvalidMinAmountOut` and `TychoRouter__AmountOutZero` |

`TychoRouter__NegativeSlippage` keeps its V3.0 meaning: the settled output fell below `minAmountOut`
and no client contribution covered the shortfall.

### V3.1 Checklist

1. Add the `amount_out` and `slippage` arguments to every `Solution::new` call; replace
   `min_amount_out()` and `with_min_amount_out()` usage.
2. Pass a non-zero simulated `amount_out` — placeholder values now fail validation.
3. Add `expectedAmountOut` to your router calldata, before `minAmountOut`.
4. Check your `minAmountOut` lands between 80% of `expectedAmountOut` and `expectedAmountOut` itself.
5. Multiply client fee rates by `10_000` and widen the field to `uint32`.
6. Re-sign client fee params against the new eleven-field `ClientFee` typehash, after encoding.
7. Replace `getEffectiveRouterFeeOnOutput`/`…Scaled` calls with `mustOutputThroughRouter`, and update
   `calculateFee` call sites to the `FeeInput` struct and single return value.
