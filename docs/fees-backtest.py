"""The in-band fee backtest of docs/fees.md §2c: replay Ethereum mainnet eth_feeHistory through the
relay's tier, repricing and hold rules and measure acceptance, inclusion, the person's price and the
relay's profit. READ-ONLY against public RPCs.

    python3 docs/fees-backtest.py fetch history.jsonl 72      # ~10 days: 72 calls of 1024 blocks
    python3 docs/fees-backtest.py run history.jsonl '{"M":1.1,"D":1.0,"caps":[1.5,1.5,1.75],
        "f":1.125,"W":20,"pt":[25,50,70],"eps":1000000,"H":12,"gas_err":0.0,"P_INCL":25}'

The model, as coded in vela-relay-core (docs/fees.md §2, §2a, §2b, §3):
  quote at block q    B = base[q+1] (the fee history's last entry); tier tips from the window ending at q
  the client pays     F = settlementGas × M × D × (cap_bps[tier] × B + tip[tier])
  submission at s=q+a the executor reads base[s] and the window ending at s; with A = F / (M × billed gas):
                        A ≥ cap_bps × b + tip          → the whole tier
                        A ≥ f × b + tip                → the cap repriced to A, the whole tip
                        A ≥ f × b + tip[slow]          → the cap A, the tip shaved to A − f × b
                        otherwise                      → held, retried on the delayed-inbox ladder (H attempts)
  inclusion proxy     the first later block whose base fee the cap covers and whose P_INCL-th percentile
                      reward the effective tip meets
  the chain charges   gas used × (base + effective tip); profit = F − that
  baseline            gas used × (B + the window's median 50th-percentile reward)
"""
import json
import os
import subprocess
import sys
import time

PCTS = [1, 5, 10, 20, 25, 30, 40, 50, 60, 70, 75, 80, 90, 95, 99]
RPCS = ['https://ethereum-rpc.publicnode.com', 'https://eth.drpc.org', 'https://eth-mainnet.public.blastapi.io']


def rpc(method, params):
    last = ''
    for attempt in range(12):
        out = subprocess.run(['curl', '-s', '-m', '60', '-X', 'POST', '-H', 'content-type: application/json', '--data',
                              json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}),
                              RPCS[attempt % len(RPCS)]], capture_output=True, text=True).stdout
        try:
            answer = json.loads(out)
            if answer.get('result'):
                return answer['result']
        except ValueError:
            pass
        last = out[:200]
        time.sleep(1)
    raise SystemExit(f'{method} failed: {last}')


def fetch(path, calls):
    """One line per block: [number, baseFeePerGas, gasUsedRatio, [reward at each of PCTS]]."""
    done = set()
    if os.path.exists(path):
        done = {json.loads(line)[0] for line in open(path)}
    newest = min(done) - 1 if done else int(rpc('eth_blockNumber', []), 16)
    with open(path, 'a') as out:
        for _ in range(calls):
            history = rpc('eth_feeHistory', [hex(1024), hex(newest), PCTS])
            oldest = int(history['oldestBlock'], 16)
            for k, ratio in enumerate(history['gasUsedRatio']):
                if oldest + k not in done:
                    rewards = [int(x, 16) for x in history['reward'][k]]
                    out.write(json.dumps([oldest + k, int(history['baseFeePerGas'][k], 16), ratio, rewards]) + '\n')
                    done.add(oldest + k)
            newest = oldest - 1
    timestamp = lambda n: int(rpc('eth_getBlockByNumber', [hex(n), False])['timestamp'], 16)
    json.dump({'lo': min(done), 'hi': max(done), 'ts_lo': timestamp(min(done)), 'ts_hi': timestamp(max(done)),
               'pcts': PCTS}, open(path + '.meta.json', 'w'))


def run(path, params):
    import numpy as np
    from numpy.lib.stride_tricks import sliding_window_view

    rows = sorted(json.loads(line) for line in open(path))
    meta = json.load(open(path + '.meta.json'))
    seconds = (meta['ts_hi'] - meta['ts_lo']) / (meta['hi'] - meta['lo'])
    base = np.array([r[1] for r in rows], dtype=float)
    reward = np.array([r[3] for r in rows], dtype=float)
    column = {p: i for i, p in enumerate(meta['pcts'])}
    n = len(rows)
    M, D, caps, f, W, pt, eps, H, gas_err = (params[k] for k in ('M', 'D', 'caps', 'f', 'W', 'pt', 'eps', 'H', 'gas_err'))
    p_incl = params.get('P_INCL', 25)

    def median(col):
        out = np.full(n, np.nan)
        out[W - 1:] = np.median(sliding_window_view(reward[:, column[col]], W), axis=1)
        return out

    paid_any = np.full(n, False)
    paid_any[W - 1:] = sliding_window_view(reward[:, column[99]] > 0, W).any(axis=1)
    slow = np.maximum(median(pt[0]), np.where(paid_any, eps, 0.0))
    tips = [slow, np.maximum(median(pt[1]), slow)]
    tips.append(np.maximum(median(pt[2]), tips[1]))
    median50 = median(50)
    ladder, t = [], 0
    for delay in ([5, 10, 20, 40, 80, 160] + [300] * 30)[:H]:
        t += delay
        ladder.append(max(1, round(t / seconds)))
    operations = {'eth_send': 146_752, 'erc20_send': 163_719, 'swap': 291_357, 'undeployed_first': 504_609}
    lookahead = 400
    result = {}
    for age, a in (('12s', 1), ('30s', 3), ('60s', 5)):
        q = np.arange(W, n - a - 1 - lookahead - ladder[-1])
        quoted_base = base[q + 1]
        for ti, tier in enumerate(('slow', 'standard', 'fast')):
            m = caps[ti]
            price = M * D * (m * quoted_base + tips[ti][q])
            funded = D * (m * quoted_base + tips[ti][q]) / (1 + gas_err)
            decided = np.zeros(len(q), bool)
            cap = np.zeros(len(q)); tip = np.zeros(len(q)); signed_at = np.zeros(len(q), int); kind = np.full(len(q), -1)
            first = None
            for j, offset in enumerate([0] + ladder):
                s = q + a + offset
                b, tier_tip, slowest = base[s], tips[ti][s], tips[0][s]
                whole = funded >= m * b + tier_tip
                repriced = ~whole & (funded >= f * b + tier_tip)
                shaved = ~whole & ~repriced & (funded >= f * b + slowest)
                ok = whole | repriced | shaved
                new = ok & ~decided
                cap[new] = np.where(whole, m * b + tier_tip, funded)[new]
                tip[new] = np.where(shaved, funded - f * b, tier_tip)[new]
                kind[new & whole] = 0; kind[new & repriced] = 1; kind[new & shaved] = 2
                signed_at[new] = s[new]
                decided |= ok
                if j == 0:
                    first = (ok.mean(), (whole | repriced).mean())
            included = np.full(len(q), -1)
            for d in range(1, lookahead):
                block = signed_at + d
                waiting = decided & (included < 0)
                if not waiting.any():
                    break
                effective = np.minimum(tip, cap - base[block])
                hit = waiting & (cap >= base[block]) & (effective >= reward[block, column[p_incl]])
                included[hit] = block[hit]
            got = included >= 0
            delay = np.where(got, included - (q + a), np.nan)
            row = dict(accept_first=first[0], full_tier_first=first[1], rejected=1 - decided.mean(),
                       shaved=(kind == 2).mean(), delay_mean=np.nanmean(delay), delay_p90=np.nanpercentile(delay, 90),
                       delay_p99=np.nanpercentile(delay, 99))
            at = np.where(got, included, 0)
            for name, used in operations.items():
                billed = used + -(-used * 1500 // 10_000) + 30_000
                paid = billed * price
                charge = used * (base[at] + np.minimum(tip, cap - base[at]))
                with np.errstate(divide='ignore', invalid='ignore'):
                    margin = np.where(got, (paid - charge) / charge, np.nan)
                row[name] = dict(paid_over_baseline=float(np.median(paid / (used * (quoted_base + median50[q])))),
                                 profit_min=float(np.nanmin(margin)), profit_median=float(np.nanmedian(margin)))
            result[f'{age}|{tier}'] = {k: (float(v) if not isinstance(v, dict) else v) for k, v in row.items()}
    return result


if __name__ == '__main__':
    if sys.argv[1] == 'fetch':
        fetch(sys.argv[2], int(sys.argv[3]) if len(sys.argv) > 3 else 72)
    else:
        print(json.dumps(run(sys.argv[2], json.loads(sys.argv[3])), indent=1))
