# Migration Guide: V3 to V3.1

This guide covers the breaking changes between Router V3.0 and V3.1 for users who consume the Rust
encoding library or build TychoRouter calldata themselves.

{% hint style="info" %}
Router V3.0 stays available on the `tycho-execution` releases that precede this change. Pin your
dependency to the last of those releases to keep using it. All later versions target V3.1.
{% endhint %}

Two changes drive everything else:

1. The router now takes **both** a quoted output amount and a minimum output amount, and bounds the
   minimum against the quote.
2. The client fee rate moved from basis points to the 8-decimal fee unit the router's own fees already
   used. That widens the field, changes the client fee typehash, and invalidates every V3.0 signature.

## Encoding Changes

### Solution: min\_amount\_out replaced by amount\_out and slippage

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

### Solution validation rejects a zero amount\_out

`validate_solution` now fails when `amount_out` is zero, because the router rejects a zero
`expectedAmountOut`. Encoding a solution with a placeholder amount — a pattern that worked when the
field only fed `minAmountOut` — returns a `FatalError`.

### ClientFeeParams: client\_fee\_bps widened to u32

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

## Execution Changes

### Router methods take expectedAmountOut

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

### The router bounds minAmountOut against expectedAmountOut

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
* Inflating `expectedAmountOut` no longer buys a looser floor — it raises the lower bound as well.
  Pass the amount your simulation returned.

The router also rejects a zero `expectedAmountOut` with `TychoRouter__AmountOutZero`.

### Fee units

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

### The client fee typehash changes

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
validates for the exact swap you produced it for.

{% hint style="warning" %}
Published V3.0 documentation described a four-field `ClientFee` struct covering only the fee
parameters. That was never accurate — V3.0 already bound the swap intent. If you implemented signing
from that description, your signatures do not verify against V3.0 either.
{% endhint %}

The EIP-712 domain is unchanged: `name = "TychoRouter"`, `version = "1"`, `verifyingContract` set to
the router address. Read the router's `CLIENT_FEE_TYPEHASH` to confirm the typehash you sign matches.

See [Client Fee Signature](encoding/#client-fee-signature) for a full signing example.

### Positive slippage and the fee base

V3.0 calculated fees from the amount the swap produced. V3.1 keeps that behaviour and adds one wrinkle:
when the router captures positive slippage, it charges fees on the output **net of** that surplus.

The router may capture output above `expectedAmountOut`, so it does not guarantee that surplus beyond
your quote reaches the receiver — amounts between `minAmountOut` and `expectedAmountOut` always do.
Query `getPositiveSlippageEnabled()` on the FeeCalculator to check whether capture is active.

The router reverts with `TychoRouter__FeesExceedOutput` if the calculated fees exceed the swap output.

### FeeCalculator interface

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

### Revert reasons

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

## Migration Checklist

1. Add the `amount_out` and `slippage` arguments to every `Solution::new` call; replace
   `min_amount_out()` and `with_min_amount_out()` usage.
2. Pass a non-zero simulated `amount_out` — placeholder values now fail validation.
3. Add `expectedAmountOut` to your router calldata, before `minAmountOut`.
4. Check your `minAmountOut` lands between 80% of `expectedAmountOut` and `expectedAmountOut` itself.
5. Multiply client fee rates by `10_000` and widen the field to `uint32`.
6. Re-sign client fee params against the new eleven-field `ClientFee` typehash, after encoding.
7. Replace `getEffectiveRouterFeeOnOutput`/`…Scaled` calls with `mustOutputThroughRouter`, and update
   `calculateFee` call sites to the `FeeInput` struct and single return value.
