------------------------------ MODULE Protocol ------------------------------
(***************************************************************************)
(* The storage protocol of docs/DESIGN.md, as a state machine.             *)
(*                                                                         *)
(* A key/value store holds one commit record (the log since the last       *)
(* checkpoint, each transaction inline or in a stored segment), stored    *)
(* segments and pages checkpointed in place. Writes are not ordered: each *)
(* is acknowledged and durable, durable without the acknowledgement, or   *)
(* lost, independently, and reads return durable values. Processes take   *)
(* turns owning the namespace; some keep an owner cache across their      *)
(* sessions (the record they saw, its epoch's pages, their last commit)   *)
(* and lose it when they crash. A ghost history records the acknowledged  *)
(* transactions, plus at most one whose outcome is unknown.               *)
(*                                                                         *)
(* Page values are transaction ids, so a page's value names the            *)
(* transaction that last wrote it, and a database state is a function      *)
(* Pages -> Nat (0: never written).                                        *)
(*                                                                         *)
(* Not modelled: the SQL engine, compression, effects (they ride in the   *)
(* record), the hint, prefetch and readahead (they change which reads     *)
(* happen, not their values), transactions of several stored segments,    *)
(* overlapping sessions of one process, and deleted pages (holes).        *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS
    Pages,       \* page numbers
    Procs,       \* processes that take turns owning the namespace
    Cached,      \* the processes that keep an owner cache
    MaxTx,       \* transactions attempted
    MaxSeg,      \* highest stored segment number
    MaxCrashes,  \* process crashes
    Nil,         \* an absent stored value (model value)
    None         \* no value (model value)

\* A request's outcome: acknowledged, durable but not acknowledged, or none.
Outcomes == {"ack", "lost", "none"}

\* The record of an empty namespace, never committed.
Empty == [version |-> 0, epoch |-> 0, log |-> <<>>, next |-> 1, garbage |-> {}]
ReadRecord(r) == IF r = Nil THEN Empty ELSE r

VARIABLES
    commitS,        \* the commit record, or Nil
    segS,           \* stored segments, by number
    pageS,          \* page keys
    pending,        \* requests in flight: [key, val]; at most one per key
    results,        \* key -> acknowledged?, for the current operation
    owner,          \* the process that owns the namespace
    ps,             \* per-process state
    truth,          \* ghost: committed transactions, in order
    unknown,        \* ghost: a transaction whose commit outcome is unknown
    txCount, crashes,
    violation       \* an open read a state the history does not allow

vars == <<commitS, segS, pageS, pending, results, owner, ps, truth, unknown,
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

\* The stored segments a record's log names.
Named(r) == {r.log[i].seg : i \in DOMAIN r.log} \ {0}

\* Replay reads each stored segment the log names: it must hold that
\* transaction.
Replays(r) == \A i \in DOMAIN r.log :
    r.log[i].seg = 0 \/ segS[r.log[i].seg] = [tx |-> r.log[i].tx, pages |-> r.log[i].pages]

(*************************************************************************)
(* Processes.                                                            *)
(*************************************************************************)

Down == [pc |-> "down",
         \* the session's view: the record it holds
         rec |-> Empty,
         op |-> [none |-> TRUE],
         \* the owner cache, which survives sessions but not a crash
         cRec |-> None, cEpoch |-> None, cCommitted |-> None,
         cPages |-> [p \in Pages |-> None]]

\* A ready session reads its log, then its cache, then the page key.
ReadPage(pr, st, p) ==
    IF InSeq(st.rec.log, p) THEN LatestIn(st.rec.log, p)
    ELSE IF pr \in Cached /\ st.cPages[p] # None THEN st.cPages[p]
    ELSE pageS[p]

View(pr, st) == [p \in Pages |-> ReadPage(pr, st, p)]

\* Forget the record: an outcome is unknown.
Forget(st) == [st EXCEPT !.cRec = None, !.cCommitted = None]

(*************************************************************************)
(* The store.                                                            *)
(*************************************************************************)

Keys(reqs) == {r.key : r \in reqs}
Settled(keys) == keys \subseteq DOMAIN results
Acked(keys) == \A k \in keys : results[k]
NoResults == [k \in {} |-> TRUE]

\* Issue requests ([key, val]); forget earlier results for their keys.
Issue(reqs) ==
    /\ pending' = pending \cup reqs
    /\ results' = [k \in DOMAIN results \ Keys(reqs) |-> results[k]]

Write(f, k, v) == [f EXCEPT ![k] = v]

\* Any request, in any order, ends one of three ways.
Resolve ==
    \E r \in pending, out \in Outcomes :
        LET k == r.key
            v == r.val
            dur == out \in {"ack", "lost"}
        IN /\ pending' = pending \ {r}
           /\ results' = [x \in DOMAIN results \cup {k} |-> IF x = k THEN out = "ack" ELSE results[x]]
           /\ commitS' = IF k[1] = "commit" /\ dur THEN v ELSE commitS
           /\ segS' = IF k[1] = "seg" /\ dur THEN Write(segS, k[2], v) ELSE segS
           /\ pageS' = IF k[1] = "page" /\ dur THEN Write(pageS, k[2], v) ELSE pageS
           /\ UNCHANGED <<owner, ps, truth, unknown, txCount, crashes, violation>>

(*************************************************************************)
(* Open: read the record and replay its log; keep cached pages of its    *)
(* epoch.                                                                *)
(*************************************************************************)

Open(pr) ==
    /\ owner = pr
    /\ pending = {}
    /\ ps[pr].pc \in {"down", "idle", "failed"}
    /\ LET st == ps[pr]
           r == ReadRecord(commitS)
           cached == pr \in Cached
           known == cached /\ st.cRec = r
           sameEpoch == cached /\ st.cEpoch = r.epoch
           opened == [st EXCEPT !.pc = "ready", !.rec = r,
                                !.cRec = IF cached THEN r ELSE None,
                                !.cEpoch = IF cached THEN r.epoch ELSE None,
                                !.cCommitted = IF ~cached THEN None
                                               ELSE IF known THEN st.cCommitted
                                               ELSE r.version,
                                !.cPages = IF sameEpoch THEN st.cPages
                                           ELSE [p \in Pages |-> None]]
           view == View(pr, opened)
           with == IF unknown = None THEN truth ELSE Append(truth, unknown)
       IN /\ ps' = [ps EXCEPT ![pr] = opened]
          \* replay reads every stored segment the record names, and the
          \* session then reads the history
          /\ IF Replays(r) /\ view = Expected(truth) THEN UNCHANGED <<truth, violation>>
             ELSE IF Replays(r) /\ unknown # None /\ view = Expected(with)
             THEN truth' = with /\ UNCHANGED violation
             ELSE violation' = TRUE /\ UNCHANGED truth
          /\ unknown' = None
    /\ UNCHANGED <<store, pending, results, owner, txCount, crashes>>

(*************************************************************************)
(* Save: one transaction, inline or in a stored segment put first; the   *)
(* record in one put, with the garbage's deletes.                        *)
(*************************************************************************)

\* One writer: with a cache, nothing committed since this view was taken.
MayWrite(pr, st) == pr \in Cached => st.cCommitted = st.rec.version

Save(pr) ==
    /\ owner = pr
    /\ pending = {}
    /\ ps[pr].pc = "ready"
    /\ txCount < MaxTx
    /\ LET st == ps[pr] IN
       /\ MayWrite(pr, st)
       /\ \E changed \in (SUBSET Pages) \ {{}}, stored \in BOOLEAN :
          LET tx == txCount + 1
              n == st.rec.next
              t == [tx |-> tx, pages |-> changed, seg |-> IF stored THEN n ELSE 0]
              segPut == IF stored THEN {[key |-> <<"seg", n>>, val |-> [tx |-> tx, pages |-> changed]]}
                        ELSE {}
          IN /\ ~stored \/ n <= MaxSeg
             /\ txCount' = tx
             /\ Issue(segPut)
             /\ ps' = [ps EXCEPT ![pr] = [st EXCEPT !.pc = "segment",
                         !.op = [t |-> t, keys |-> Keys(segPut)]]]
    /\ UNCHANGED <<store, owner, truth, unknown, crashes, violation>>

\* The segment settled: the record names it only once it is acknowledged.
CommitRecord(pr) ==
    /\ ps[pr].pc = "segment"
    /\ Settled(ps[pr].op.keys)
    /\ LET st == ps[pr]
           t == st.op.t
           r == [st.rec EXCEPT !.version = @ + 1, !.log = Append(@, t),
                               !.next = IF t.seg = 0 THEN @ ELSE @ + 1]
           puts == {[key |-> <<"commit">>, val |-> r]}
           deletes == {[key |-> <<"seg", s>>, val |-> Nil] : s \in st.rec.garbage}
       IN IF ~Acked(st.op.keys)
          THEN \* nothing committed: the session fails
               /\ ps' = [ps EXCEPT ![pr] = [Forget(st) EXCEPT !.pc = "failed", !.op = [none |-> TRUE]]]
               /\ results' = NoResults
               /\ UNCHANGED pending
          ELSE /\ Issue(puts \cup deletes)
               /\ ps' = [ps EXCEPT ![pr] = [st EXCEPT !.pc = "commit",
                           !.op = [t |-> t, rec |-> r, commit |-> Keys(puts), deletes |-> Keys(deletes)]]]
    /\ UNCHANGED <<store, owner, truth, unknown, txCount, crashes, violation>>

Committed(pr) ==
    /\ ps[pr].pc = "commit"
    /\ LET st == ps[pr]
           op == st.op
       IN
       /\ Settled(op.commit \cup op.deletes)
       /\ results' = NoResults
       /\ IF ~Acked(op.commit)
          THEN \* the outcome is unknown
               /\ unknown' = [tx |-> op.t.tx, pages |-> op.t.pages]
               /\ ps' = [ps EXCEPT ![pr] = [Forget(st) EXCEPT !.pc = "failed", !.op = [none |-> TRUE]]]
               /\ UNCHANGED truth
          ELSE \* durable; acknowledged deletes leave the next record's garbage
               LET r == [op.rec EXCEPT !.garbage = IF Acked(op.deletes) THEN {} ELSE @]
               IN /\ truth' = Append(truth, [tx |-> op.t.tx, pages |-> op.t.pages])
                  /\ ps' = [ps EXCEPT ![pr] = [st EXCEPT !.pc = "ready", !.rec = r,
                              !.op = [none |-> TRUE],
                              !.cRec = IF pr \in Cached THEN op.rec ELSE None,
                              !.cCommitted = IF pr \in Cached THEN r.version ELSE None]]
                  /\ UNCHANGED unknown
    /\ UNCHANGED <<store, pending, owner, txCount, crashes, violation>>

(*************************************************************************)
(* Checkpoint: each page's latest image in the log; once all are          *)
(* acknowledged, a record with an empty log at the next epoch.           *)
(*************************************************************************)

Checkpoint(pr) ==
    /\ owner = pr
    /\ pending = {}
    /\ ps[pr].pc = "ready"
    /\ LET st == ps[pr] IN
       /\ st.rec.log # <<>>
       /\ MayWrite(pr, st)
       /\ LET puts == {[key |-> <<"page", p>>, val |-> LatestIn(st.rec.log, p)] : p \in LogPages(st.rec.log)}
          IN /\ Issue(puts)
             /\ ps' = [ps EXCEPT ![pr] = [st EXCEPT !.pc = "pages", !.op = [keys |-> Keys(puts)]]]
    /\ UNCHANGED <<store, owner, truth, unknown, txCount, crashes, violation>>

CheckpointRecord(pr) ==
    /\ ps[pr].pc = "pages"
    /\ Settled(ps[pr].op.keys)
    /\ LET st == ps[pr]
           r == [st.rec EXCEPT !.epoch = @ + 1, !.log = <<>>, !.garbage = @ \cup Named(st.rec)]
           puts == {[key |-> <<"commit">>, val |-> r]}
       IN IF ~Acked(st.op.keys)
          THEN \* a cleanup error: the log keeps covering the pages
               /\ ps' = [ps EXCEPT ![pr] = [st EXCEPT !.pc = "ready", !.op = [none |-> TRUE]]]
               /\ results' = NoResults
               /\ UNCHANGED pending
          ELSE /\ Issue(puts)
               /\ ps' = [ps EXCEPT ![pr] = [st EXCEPT !.pc = "record",
                           !.op = [keys |-> Keys(puts), rec |-> r]]]
    /\ UNCHANGED <<store, owner, truth, unknown, txCount, crashes, violation>>

CheckpointRecorded(pr) ==
    /\ ps[pr].pc = "record"
    /\ Settled(ps[pr].op.keys)
    /\ LET st == ps[pr]
           r == st.op.rec
           cached == pr \in Cached
           deletes == {[key |-> <<"seg", s>>, val |-> Nil] : s \in r.garbage}
       IN IF ~Acked(st.op.keys)
          THEN \* it may have landed: the session continues on its log, which
               \* covers the pages either way, and forgets the record
               /\ ps' = [ps EXCEPT ![pr] = [Forget(st) EXCEPT !.pc = "ready", !.op = [none |-> TRUE]]]
               /\ results' = NoResults
               /\ UNCHANGED pending
          ELSE \* the next epoch: the cache keeps the previous one's pages, but
               \* those of the log, which it writes through. No durable record
               \* names the garbage now: delete it.
               /\ Issue(deletes)
               /\ ps' = [ps EXCEPT ![pr] = [st EXCEPT !.pc = "collect", !.rec = r,
                           !.op = [keys |-> Keys(deletes)],
                           !.cRec = IF cached THEN r ELSE None,
                           !.cEpoch = IF cached THEN r.epoch ELSE None,
                           !.cCommitted = IF cached THEN r.version ELSE None,
                           !.cPages = [p \in Pages |->
                               IF cached /\ p \in LogPages(st.rec.log) THEN LatestIn(st.rec.log, p)
                               ELSE IF cached /\ st.cEpoch = st.rec.epoch THEN st.cPages[p]
                               ELSE None]]]
    /\ UNCHANGED <<store, owner, truth, unknown, txCount, crashes, violation>>

\* Acknowledged deletes leave the next record's garbage.
Collected(pr) ==
    /\ ps[pr].pc = "collect"
    /\ Settled(ps[pr].op.keys)
    /\ results' = NoResults
    /\ LET st == ps[pr] IN
       ps' = [ps EXCEPT ![pr] = [st EXCEPT !.pc = "ready", !.op = [none |-> TRUE],
                 !.rec = [st.rec EXCEPT !.garbage = IF Acked(st.op.keys) THEN {} ELSE @]]]
    /\ UNCHANGED <<store, pending, owner, truth, unknown, txCount, crashes, violation>>

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
    /\ UNCHANGED <<store, pending, results, owner, truth, unknown, txCount, crashes, violation>>

EndSession(pr) ==
    /\ ps[pr].pc = "ready"
    /\ ps' = [ps EXCEPT ![pr].pc = "idle"]
    /\ UNCHANGED <<store, pending, results, owner, truth, unknown, txCount, crashes, violation>>

\* Between operations: nothing in flight, no result unprocessed.
Stable == pending = {} /\ \A pr \in Procs : ps[pr].pc \in {"down", "idle", "ready", "failed"}

EndSessions == [pr \in Procs |-> IF ps[pr].pc \in {"ready", "failed"}
                                 THEN [ps[pr] EXCEPT !.pc = "idle"] ELSE ps[pr]]

\* Ownership moves only after every request settled.
Handover ==
    /\ Stable
    /\ \E q \in Procs \ {owner} : owner' = q
    /\ ps' = EndSessions
    /\ UNCHANGED <<store, pending, results, truth, unknown, txCount, crashes, violation>>

\* A process crashes and loses its cache.
CrashProc ==
    /\ crashes < MaxCrashes
    /\ Stable
    /\ \E pr \in Procs : ps[pr].pc # "down" /\ ps' = [ps EXCEPT ![pr] = Down]
    /\ crashes' = crashes + 1
    /\ UNCHANGED <<store, pending, results, owner, truth, unknown, txCount, violation>>

(*************************************************************************)
(* Specification and properties.                                         *)
(*************************************************************************)

Init ==
    /\ commitS = Nil
    /\ segS = [s \in 1..MaxSeg |-> Nil]
    /\ pageS = [p \in Pages |-> 0]
    /\ pending = {} /\ results = NoResults
    /\ owner \in Procs
    /\ ps = [pr \in Procs |-> Down]
    /\ truth = <<>> /\ unknown = None
    /\ txCount = 0 /\ crashes = 0 /\ violation = FALSE

Next ==
    \/ Resolve
    \/ \E pr \in Procs : Open(pr) \/ Save(pr) \/ CommitRecord(pr) \/ Committed(pr)
                         \/ Checkpoint(pr) \/ CheckpointRecord(pr) \/ CheckpointRecorded(pr)
                         \/ Collected(pr)
                         \/ Read(pr) \/ EndSession(pr)
    \/ Handover \/ CrashProc

Spec == Init /\ [][Next]_vars

(* Every open reads the history: each acknowledged transaction, in order, *)
(* and the one with an unknown outcome or not; never part of one, never  *)
(* one that a previous open decided was not committed; and replay finds   *)
(* every stored segment the record names.                                 *)
OpensReadTheHistory == ~violation

(* A ready session reads the history at every moment, through its log,    *)
(* its cache and the page keys: no checkpoint exposes a stale page.       *)
ReadyReadsTheHistory ==
    \A pr \in Procs : (ps[pr].pc = "ready" /\ owner = pr) => View(pr, ps[pr]) = Expected(truth)
=============================================================================
