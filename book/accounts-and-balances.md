# 3. Accounts and balances

The previous chapter separated chain state from wallet state. This chapter
explains what the wallet is counting. When you later send 1 ZEC, you should be
able to predict **which account and pool** will change, and why a displayed
total is not always spendable.

## An account is a wallet identity

The app initializes six deterministic wallet accounts from one disposable
seed. Accounts **1–5** are development accounts. Account **6** is a hidden
treasury used for mining rewards and faucet payments. The accounts API, Wallet
page, account selector, and user-facing balances expose only 1–5.

Each user account has a transparent address and a unified address. For the
flows in this book, selecting the **Transparent** destination pool uses the
transparent address; selecting **Orchard** uses the unified address for a
shielded output. The server derives both from the instance's seed.

That seed is **fixed**: the server builds it from 32 zero bytes, which makes
the mnemonic the well-known `abandon abandon … art` phrase. Every instance,
on every machine, therefore derives the *same* six accounts and the same
addresses — Account 1's unified address always begins `uregtest1zkuzfv5m3…`.
A different instance name gives you a different chain and different balances;
it does **not** give you different keys.

Say that plainly: these keys are public knowledge. Anyone who has read this
project can derive every address and every spending key your instance uses.
That is fine, because it is what makes the environment disposable and
reproducible — and it is exactly why nothing of value may ever touch these
addresses. Never send real ZEC to an address shown by this environment.

A **pool** is a way value is represented and spent, not a separate wallet.
Transparent value lives in unspent transaction outputs, or **UTXOs**; their
addresses and amounts are public chain data. Orchard value lives in shielded
**notes**. The wallet learns which notes belong to it by scanning compact
blocks; the public explorer cannot identify their recipient or read a memo.

![Mining pays every block reward to hidden Account 6's transparent balance; shielding a matured reward moves it to Account 6's Orchard balance, which is the pool every faucet payment spends from into accounts 1 to 5.](images/account-pools.svg)

1. Each user account can hold a transparent balance and an Orchard balance.
   These are separate sources of funds, even though the dashboard also shows
   their sum.
2. The miner sends block rewards to Account 6's transparent address. Account
   6 never appears in a user account list or balance total.
3. The faucet can spend from Account 6 to a chosen user account and pool.
   A send between users spends the selected **source** account and pool and
   pays to the selected **destination** account and pool.

## ZEC, zatoshi, and spendability

**ZEC** is the amount unit you enter in the UI and CLI. **Zatoshi** is the
integer unit used by the API: **1 ZEC = 100,000,000 zatoshi**. For example,
`0.5` ZEC is `50000000` zatoshi. Use integer zatoshi in JSON requests so a
decimal floating-point calculation does not choose the wrong amount.

The accounts API returns `transparent_zatoshi` and `orchard_zatoshi` for
each user account. These are the server's last reconciled **wallet
snapshot**, not numbers reconstructed from the block explorer. The dashboard
derives its displayed total from those fields.

A balance is not a promise that the next transaction can spend every zatoshi.
The wallet SDK selects actual outputs or notes, requires a sufficient scan,
and accounts for the transaction fee. Pending change and immature mining
rewards can also affect spendability. **Change** is the unspent remainder of a
selected input returned to the wallet; it may not be ready to spend
immediately.

You do not have to guess how much room the fee needs. `POST /api/v1/send/quote`
builds a **dry-run proposal** for one account and pool pair — it constructs
nothing and broadcasts nothing — and answers with real numbers:

```console
$ DASHBOARD_URL="$(ths endpoints --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["dashboard"])')"
$ curl -fsS "$DASHBOARD_URL/api/v1/send/quote" -H 'content-type: application/json' \
    --data '{"from_account":1,"source_pool":"orchard","destination_pool":"orchard"}'
{"available_zatoshi":399990000,"fee_zatoshi":10000,"max_zatoshi":399980000}
```

`available_zatoshi` is what the pool holds, `fee_zatoshi` is what the SDK's
input selection will actually charge, and `max_zatoshi` is the largest amount
that will go through. `max_zatoshi`, not the displayed balance, is the number
to compare an intended amount against. When a send fails anyway, check the
**source pool** and inspect wallet sync status before trying again.

At the end of startup, Account 1 has **5 Orchard ZEC**. Account 2 starts
empty. In the next chapter, we will give Account 2 transparent funds and
then send it Orchard funds from Account 1. That will make the two balances
visibly different.

## Where faucet funds come from

A faucet request does not mint coins directly into the destination. The
miner first pays rewards to the hidden treasury's transparent address. The
reward created by mining a block is called **coinbase**; it must mature
before it can be spent. When the treasury's spendable Orchard funds are
insufficient, the server discovers needed mature rewards, shields them into
the treasury's Orchard pool, confirms that shielding transaction, and retries
the faucet payment. This can move the node height by more than the one block
used to confirm the final faucet payment.

![A faucet request proposes a payment from the treasury's Orchard pool. With enough spendable funds it broadcasts, records and confirms. Otherwise it mines for maturity, discovers and shields a reward, and retries the proposal.](images/treasury-funding.svg)

1. Zakura mines to Account 6's transparent address. The rewards are not
   user-account balances.
2. The wallet SDK first tries the treasury payment. On insufficient
   spendable Orchard funds, the server advances the chain as needed for reward
   maturity, discovers the required reward output, and shields it to Account
   6's Orchard funds. It retries the payment proposal.
3. The server broadcasts the faucet transaction to the chosen user account and
   destination pool, records activity, and normally mines a
   confirming block. Each account faucet request is limited to **5 ZEC**.

Routine user wallet synchronization does not query all treasury transparent
outputs. Treasury discovery runs when replenishment needs it. The private
details stay out of the user account list, but the public chain may still
show transparent mining rewards and shielding value movement.

## Three views of the same experiment

After a payment, these views can temporarily disagree without being
contradictory:

| View | What it can establish |
| --- | --- |
| Network or Zakura RPC | Which block is the current node tip. |
| Recent activity | Which request and transaction ID the server recorded, and whether it has a confirming block hash. |
| Wallet account balance | What the server's reconciled wallet scan says the user account holds. |

Read [Chapter 5](mining-and-sync.md) for the precise order in which those
views catch up. The [architecture chapter](architecture/overview.md#the-five-owners-of-information)
maps each view to its owner.

**Next:** [fund Account 2 and send between pools](using-the-dashboard.md).
