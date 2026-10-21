------------------------------ MODULE Protocol ------------------------------
(***************************************************************************)
(* The storage protocol of docs/DESIGN.md, as a state machine.             *)
(*                                                                         *)
(* A key/value store holds one commit record (the log since the last       *)
(* checkpoint, each transaction inline or in a stored value), stored      *)
(* values and pages checkpointed in place. The store is a Restate         *)
(* object's state: puts and deletes are queued in issue order, a sync     *)
(* waits until every write before it is durable, and a failure (or a      *)
(* crash) keeps a prefix of the writes not yet synced, never a later one. *)
(*                                                                         *)
(* A save queues, in order: the deletes of the durable record's garbage,  *)
(* a large transaction's stored value, a seal of the record's inline      *)
(* transactions, the record naming both, and, if the log is long, a       *)
(* checkpoint (each page's latest image, then a record with an empty log  *)
(* at the next epoch). Then one sync.                                      *)
(*                                                                         *)
(* Processes take turns owning the namespace; some keep an owner cache    *)
(* across their sessions (the record they saw, their last commit, its     *)
(* epoch's pages and the stored values it names) and lose it when they    *)
(* crash. A ghost history records the acknowledged transactions, plus at  *)
(* most one whose outcome is unknown.                                      *)
(*                                                                         *)
(* Page values are transaction ids, so a page's value names the            *)
(* transaction that last wrote it, and a database state is a function      *)
(* Pages -> Nat (0: never written).                                        *)
(*                                                                         *)
(* Not modelled: the SQL engine, compression and records                   *)
(* (they ride in the record), the hint, prefetch and readahead (they       *)
(* change which reads happen, not their values), transactions of several  *)
(* stored values, overlapping sessions of one process, and deleted pages. *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS
    Pages,       \* page numbers
    Procs,       \* processes that take turns owning the namespace
    Cached,      \* the processes that keep an owner cache
    MaxTx,       \* transactions attempted
    MaxSeg,      \* highest stored value number
    MaxCrashes,  \* process crashes
    Nil,         \* an absent stored value (model value)
    None         \* no value (model value)

\* The record of an empty namespace, never committed.
Empty == [version |-> 0, epoch |-> 0, log |-> <<>>, next |-> 1, garbage |-> {}]
ReadRecord(r) == IF r = Nil THEN Empty ELSE r

VARIABLES
    commitS,        \* the commit record, or Nil
    segS,           \* stored values, by number: sets of transactions
    pageS,          \* page keys
    queue,          \* writes not yet durable, in issue order: [key, val]
    owner,          \* the process that owns the namespace
    ps,             \* per-process state
    truth,          \* ghost: committed transactions, in order
    unknown,        \* ghost: a transaction whose commit outcome is unknown
    txCount, crashes,
    violation       \* an open read a state the history does not allow

vars == <<commitS, segS, pageS, queue, owner, ps, truth, unknown,
          txCount, crashes, violation>>

store == <<commitS, segS, pageS>>

(*************************************************************************)
(* Transactions and states.                                              *)
(*************************************************************************)

\* The value of page p after a sequence of transactions (records with
\* fields tx and pages): the last one that changed it, else 0.
RECURSIVE LatestIn(_, _)
LatestIn(seq, p) ==
    IF seq = <<>> THEN 0
    ELSE IF p \in seq[Len(seq)].pages THEN seq[Len(seq)].tx
    ELSE LatestIn(SubSeq(seq, 1, Len(seq) - 1), p)

InSeq(seq, p) == \E i \in DOMAIN seq : p \in seq[i].pages

Expected(seq) == [p \in Pages |-> LatestIn(seq, p)]

LogPages(log) == UNION {log[i].pages : i \in DOMAIN log}

\* The stored values a record's log names.
Named(r) == {r.log[i].seg : i \in DOMAIN r.log} \ {0}

Tx(t) == [tx |-> t.tx, pages |-> t.pages]

\* The record's inline transactions.
InlineTx(r) == {Tx(r.log[i]) : i \in {j \in DOMAIN r.log : r.log[j].seg = 0}}

\* Name the seal `n`, holding `txs`, in place of those inline transactions.
Rename(r, n, txs) ==
    [r EXCEPT !.log = [i \in DOMAIN r.log |->
                         IF r.log[i].seg = 0 /\ Tx(r.log[i]) \in txs
                         THEN [r.log[i] EXCEPT !.seg = n] ELSE r.log[i]]]

\* A set of numbers as a sequence, ascending.
RECURSIVE Ascending(_)
Ascending(s) ==
    IF s = {} THEN <<>>
    ELSE LET x == CHOOSE x \in s : \A y \in s : x <= y
         IN <<x>> \o Ascending(s \ {x})

Put(key, val) == [key |-> key, val |-> val]

(*************************************************************************)
(* Processes.                                                            *)
(*************************************************************************)

NoValues == [n \in 1..MaxSeg |-> None]

Down == [pc |-> "down",
         \* the session's view: the record it holds
         rec |-> Empty,
         op |-> [none |-> TRUE],
         \* the owner cache, which survives sessions but not a crash
         cRec |-> None, cEpoch |-> None, cCommitted |-> None,
         cPages |-> [p \in Pages |-> None],
         cValues |-> NoValues]

\* A ready session reads its log, then its cache, then the page key.
ReadPage(pr, st, p) ==
    IF InSeq(st.rec.log, p) THEN LatestIn(st.rec.log, p)
    ELSE IF pr \in Cached /\ st.cPages[p] # None THEN st.cPages[p]
    ELSE pageS[p]

View(pr, st) == [p \in Pages |-> ReadPage(pr, st, p)]

\* A stored value, as this process reads it: from its cache, else the store.
Value(pr, st, n) ==
    IF pr \in Cached /\ st.cValues[n] # None THEN st.cValues[n] ELSE segS[n]

\* Replay reads each stored value the log names: it must hold that
\* transaction.
Replays(pr, st, r) == \A i \in DOMAIN r.log :
    r.log[i].seg = 0 \/ (Value(pr, st, r.log[i].seg) # Nil
                         /\ Tx(r.log[i]) \in Value(pr, st, r.log[i].seg))

\* The values the cache keeps: those the durable record `r` names.
Keep(values, r) == [n \in 1..MaxSeg |-> IF n \in Named(r) THEN values[n] ELSE None]

\* A commit's outcome is unknown: forget the record. Pages and values stay:
\* a checkpoint that landed moved the epoch, which the next open sees, and a
\* value a durable record named is never written again.
Forget(st) == [st EXCEPT !.cRec = None, !.cCommitted = None]

(*************************************************************************)
(* The store: the queue's head lands, at any time.                       *)
(*************************************************************************)

Write(f, k, v) == [f EXCEPT ![k] = v]

Land ==
    /\ queue # <<>>
    /\ LET k == Head(queue).key
           v == Head(queue).val
       IN /\ commitS' = IF k[1] = "commit" THEN v ELSE commitS
          /\ segS' = IF k[1] = "seg" THEN Write(segS, k[2], v) ELSE segS
          /\ pageS' = IF k[1] = "page" THEN Write(pageS, k[2], v) ELSE pageS
    /\ queue' = Tail(queue)
    /\ UNCHANGED <<owner, ps, truth, unknown, txCount, crashes, violation>>

(*************************************************************************)
(* Open: read the record and replay its log; keep cached pages of its    *)
(* epoch.                                                                *)
(*************************************************************************)

Open(pr) ==
    /\ owner = pr
    /\ queue = <<>>
    /\ ps[pr].pc \in {"down", "idle", "failed"}
    /\ LET st == ps[pr]
           r == ReadRecord(commitS)
           cached == pr \in Cached
           known == cached /\ st.cRec = r
           sameEpoch == cached /\ st.cEpoch = r.epoch
           opened == [st EXCEPT !.pc = "ready",
                                !.rec = r,
                                !.cRec = IF cached THEN r ELSE None,
                                !.cEpoch = IF cached THEN r.epoch ELSE None,
                                !.cCommitted = IF ~cached THEN None
                                               ELSE IF known THEN st.cCommitted
                                               ELSE r.version,
                                !.cPages = IF sameEpoch THEN st.cPages
                                           ELSE [p \in Pages |-> None],
                                !.cValues = IF cached
                                            THEN Keep([n \in 1..MaxSeg |-> Value(pr, st, n)], r)
                                            ELSE NoValues]
           view == View(pr, opened)
           with == IF unknown = None THEN truth ELSE Append(truth, unknown)
       IN /\ ps' = [ps EXCEPT ![pr] = opened]
          \* replay reads every stored value the record names, and the
          \* session then reads the history
          /\ IF Replays(pr, st, r) /\ view = Expected(truth) THEN UNCHANGED <<truth, violation>>
             ELSE IF Replays(pr, st, r) /\ unknown # None /\ view = Expected(with)
             THEN truth' = with /\ UNCHANGED violation
             ELSE violation' = TRUE /\ UNCHANGED truth
          /\ unknown' = None
    /\ UNCHANGED <<store, queue, owner, txCount, crashes>>

(*************************************************************************)
(* Save: one transaction, its writes queued in order, then one sync.     *)
(*************************************************************************)

\* One writer: with a cache, nothing committed since this view was taken.
MayWrite(pr, st) == pr \in Cached => st.cCommitted = st.rec.version

\* The checkpoint of record r: a record with an empty log at the next epoch,
\* whose garbage takes the values the log named.
CheckpointOf(r) == [r EXCEPT !.epoch = @ + 1, !.log = <<>>, !.garbage = @ \cup Named(r)]

Save(pr) ==
    /\ owner = pr
    /\ queue = <<>>
    /\ ps[pr].pc = "ready"
    /\ txCount < MaxTx
    /\ LET st == ps[pr] IN
       /\ MayWrite(pr, st)
       /\ \E changed \in (SUBSET Pages) \ {{}}, stored \in BOOLEAN, seal \in BOOLEAN,
             checkpoint \in BOOLEAN :
          LET old == st.rec
              tx == txCount + 1
              \* 1. the durable record's garbage
              deletes == [i \in DOMAIN Ascending(old.garbage) |->
                            Put(<<"seg", Ascending(old.garbage)[i]>>, Nil)]
              \* 2. a large transaction's value, then a seal of the inline ones
              n == old.next
              t == [tx |-> tx, pages |-> changed, seg |-> IF stored THEN n ELSE 0]
              m == n + (IF stored THEN 1 ELSE 0)
              sealing == seal /\ InlineTx(old) # {} /\ m <= MaxSeg
              values == [k \in 1..MaxSeg |->
                           IF stored /\ k = n THEN {Tx(t)}
                           ELSE IF sealing /\ k = m THEN InlineTx(old)
                           ELSE None]
              puts == (IF stored THEN <<Put(<<"seg", n>>, {Tx(t)})>> ELSE <<>>)
                      \o (IF sealing THEN <<Put(<<"seg", m>>, InlineTx(old))>> ELSE <<>>)
              \* 3. the record, listing as garbage what it names no more
              base == IF sealing THEN Rename(old, m, InlineTx(old)) ELSE old
              r0 == [base EXCEPT !.version = @ + 1, !.log = Append(@, t),
                                 !.next = m + (IF sealing THEN 1 ELSE 0)]
              r == [r0 EXCEPT !.garbage = Named(old) \ Named(r0)]
              \* 4. a checkpoint: each page's latest image, then its record
              c == CheckpointOf(r)
              pages == [i \in DOMAIN Ascending(LogPages(r.log)) |->
                          LET p == Ascending(LogPages(r.log))[i]
                          IN Put(<<"page", p>>, LatestIn(r.log, p))]
          IN /\ ~stored \/ n <= MaxSeg
             /\ txCount' = tx
             /\ queue' = deletes \o puts \o <<Put(<<"commit">>, r)>>
                         \o (IF checkpoint THEN pages \o <<Put(<<"commit">>, c)>> ELSE <<>>)
             /\ ps' = [ps EXCEPT ![pr] = [st EXCEPT !.pc = "syncing",
                         !.op = [t |-> t, rec |-> r, checkpoint |-> checkpoint,
                                 final |-> IF checkpoint THEN c ELSE r, values |-> values]]]
    /\ UNCHANGED <<store, owner, truth, unknown, crashes, violation>>

\* The sync is acknowledged: every write landed.
Synced(pr) ==
    /\ ps[pr].pc = "syncing"
    /\ queue = <<>>
    /\ LET st == ps[pr]
           op == st.op
           r == op.final
           cached == pr \in Cached
           \* the cache keeps the epoch's pages; a checkpoint moves them to
           \* the next epoch and replaces those of its log
           keepPages == cached /\ st.cEpoch = st.rec.epoch
           held == [n \in 1..MaxSeg |-> IF op.values[n] # None THEN op.values[n]
                                        ELSE st.cValues[n]]
       IN /\ truth' = Append(truth, Tx(op.t))
          /\ ps' = [ps EXCEPT ![pr] = [st EXCEPT !.pc = "ready", !.rec = r,
                      !.op = [none |-> TRUE],
                      !.cRec = IF cached THEN r ELSE None,
                      !.cEpoch = IF cached THEN r.epoch ELSE None,
                      !.cCommitted = IF cached THEN r.version ELSE None,
                      !.cPages = [p \in Pages |->
                          IF cached /\ op.checkpoint /\ p \in LogPages(op.rec.log)
                          THEN LatestIn(op.rec.log, p)
                          ELSE IF keepPages THEN st.cPages[p]
                          ELSE None],
                      !.cValues = IF cached THEN Keep(held, r) ELSE NoValues]]
    /\ UNCHANGED <<store, queue, owner, unknown, txCount, crashes, violation>>

\* The sync fails, or the process dies during it: the writes still queued are
\* lost (Restate never applies an abandoned attempt's commands later), so a
\* prefix landed. The outcome is unknown.
Failed(pr) ==
    /\ ps[pr].pc = "syncing"
    /\ queue' = <<>>
    /\ unknown' = Tx(ps[pr].op.t)
    /\ ps' = [ps EXCEPT ![pr] = [Forget(ps[pr]) EXCEPT !.pc = "failed", !.op = [none |-> TRUE]]]
    /\ UNCHANGED <<store, owner, truth, txCount, crashes, violation>>

(*************************************************************************)
(* Reads, session ends, handover and crashes.                            *)
(*************************************************************************)

\* A read of a page outside the log caches the page key's value.
Read(pr) ==
    /\ pr \in Cached
    /\ owner = pr
    /\ ps[pr].pc = "ready"
    /\ \E p \in Pages :
        /\ ~InSeq(ps[pr].rec.log, p)
        /\ ps[pr].cPages[p] = None
        /\ ps' = [ps EXCEPT ![pr].cPages[p] = pageS[p]]
    /\ UNCHANGED <<store, queue, owner, truth, unknown, txCount, crashes, violation>>

EndSession(pr) ==
    /\ ps[pr].pc = "ready"
    /\ ps' = [ps EXCEPT ![pr].pc = "idle"]
    /\ UNCHANGED <<store, queue, owner, truth, unknown, txCount, crashes, violation>>

\* Between operations: nothing queued, no sync waiting.
Stable == queue = <<>> /\ \A pr \in Procs : ps[pr].pc \in {"down", "idle", "ready", "failed"}

EndSessions == [pr \in Procs |-> IF ps[pr].pc \in {"ready", "failed"}
                                 THEN [ps[pr] EXCEPT !.pc = "idle"] ELSE ps[pr]]

\* Ownership moves only once the owner's attempt is over.
Handover ==
    /\ Stable
    /\ \E q \in Procs \ {owner} : owner' = q
    /\ ps' = EndSessions
    /\ UNCHANGED <<store, queue, truth, unknown, txCount, crashes, violation>>

\* A process crashes and loses its cache (a crash during a sync is a
\* failure first).
CrashProc ==
    /\ crashes < MaxCrashes
    /\ Stable
    /\ \E pr \in Procs : ps[pr].pc # "down" /\ ps' = [ps EXCEPT ![pr] = Down]
    /\ crashes' = crashes + 1
    /\ UNCHANGED <<store, queue, owner, truth, unknown, txCount, violation>>

(*************************************************************************)
(* Specification and properties.                                         *)
(*************************************************************************)

Init ==
    /\ commitS = Nil
    /\ segS = [s \in 1..MaxSeg |-> Nil]
    /\ pageS = [p \in Pages |-> 0]
    /\ queue = <<>>
    /\ owner \in Procs
    /\ ps = [pr \in Procs |-> Down]
    /\ truth = <<>> /\ unknown = None
    /\ txCount = 0 /\ crashes = 0 /\ violation = FALSE

Next ==
    \/ Land
    \/ \E pr \in Procs : Open(pr) \/ Save(pr) \/ Synced(pr) \/ Failed(pr)
                         \/ Read(pr) \/ EndSession(pr)
    \/ Handover \/ CrashProc

Spec == Init /\ [][Next]_vars

(* Every open reads the history: each acknowledged transaction, in order, *)
(* and the one with an unknown outcome or not; never part of one, never  *)
(* one that a previous open decided was not committed; and replay finds   *)
(* every stored value the record names.                                   *)
OpensReadTheHistory == ~violation

(* No stored value leaks: each one present is named by the durable record, *)
(* listed as its garbage, or at a number it has not allocated (which a    *)
(* later save overwrites).                                                 *)
NothingLeaks ==
    LET r == ReadRecord(commitS)
    IN \A n \in 1..MaxSeg : segS[n] # Nil => n \in Named(r) \cup r.garbage \/ n >= r.next

(* A ready session reads the history at every moment, through its log,    *)
(* its cache and the page keys: no checkpoint exposes a stale page.       *)
ReadyReadsTheHistory ==
    \A pr \in Procs : (ps[pr].pc = "ready" /\ owner = pr) => View(pr, ps[pr]) = Expected(truth)
=============================================================================
