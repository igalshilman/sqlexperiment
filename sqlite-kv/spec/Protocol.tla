------------------------------ MODULE Protocol ------------------------------
(***************************************************************************)
(* The storage protocol of docs/DESIGN.md, as a state machine.             *)
(*                                                                         *)
(* A key/value store with uncertain outcomes holds a log of transactions,  *)
(* pages checkpointed in place and a checkpoint record (meta). Processes   *)
(* take turns owning the namespace. Each keeps an owner cache across its   *)
(* sessions (the meta it saw, a claim, its last commit, cached pages) and  *)
(* loses it when it crashes. A ghost history records the acknowledged      *)
(* transactions, plus at most one whose outcome is unknown.                *)
(*                                                                         *)
(* Page values are transaction ids, so a page's value names the            *)
(* transaction that last wrote it, and a database state is a function      *)
(* Pages -> Nat (0: never written).                                        *)
(*                                                                         *)
(* Not modelled: the SQL engine, compression, the hint, prefetch and       *)
(* readahead (they change which reads happen, not their values), large    *)
(* self-checkpointing saves, overlapping sessions of one process, and      *)
(* deleted pages (holes).                                                  *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS
    Pages,       \* page numbers
    Procs,       \* processes that take turns owning the namespace
    MaxTx,       \* transactions attempted
    MaxSeq,      \* highest log sequence number
    MaxCrashes,  \* process crashes plus store losses
    UseCache,    \* processes keep an owner cache
    Weak,        \* a failed put may be visible without being durable
    Nil,         \* an absent stored value (model value)
    None,        \* no value (model value)
    Unknown      \* meta bytes that match no stored record (model value)

DefaultMeta == [start |-> 1, del |-> 1]
ReadMeta(m) == IF m = Nil THEN DefaultMeta ELSE m

\* A request's outcome. "lost": durable but not acknowledged.
Outcomes == IF Weak THEN {"ack", "lost", "visible", "none"} ELSE {"ack", "lost", "none"}

VARIABLES
    logV, logD,     \* visible and durable log segments, by sequence
    pageV, pageD,   \* visible and durable page keys
    metaV, metaD,   \* visible and durable meta
    pending,        \* requests in flight: [key, val]; at most one per key
    results,        \* key -> acknowledged?, for the current operation
    owner,          \* the process that owns the namespace
    ps,             \* per-process state
    truth,          \* ghost: committed transactions, in order
    unknown,        \* ghost: a transaction whose commit outcome is unknown
    txCount, crashes,
    violation       \* an open read a state the history does not allow

vars == <<logV, logD, pageV, pageD, metaV, metaD, pending, results,
          owner, ps, truth, unknown, txCount, crashes, violation>>

store == <<logV, logD, pageV, pageD, metaV, metaD>>

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

(* Replay from sequence s: apply a transaction at its commit segment; stop *)
(* at an absent segment or one that does not continue the transaction.   *)
RECURSIVE Replay(_, _, _, _)
Replay(s, next, pend, acc) ==
    IF s > MaxSeq \/ logV[s] = Nil \/ logV[s].first # next
    THEN [txs |-> acc, next |-> next]
    ELSE IF logV[s].commit
    THEN Replay(s + 1, s + 1, {},
                Append(acc, [end |-> s + 1, tx |-> logV[s].tx, pages |-> pend \cup logV[s].pages]))
    ELSE Replay(s + 1, next, pend \cup logV[s].pages, acc)

(* The latest images of the transactions in [s, to), as open reads them  *)
(* to refresh the cache; Nil if a segment is gone or the range is not    *)
(* whole transactions.                                                   *)
NoChange == [p \in Pages |-> None]
RECURSIVE Changes(_, _, _, _)
Changes(s, to, first, acc) ==
    IF s = to THEN (IF first = to THEN acc ELSE Nil)
    ELSE IF s > MaxSeq \/ logV[s] = Nil \/ logV[s].first # first THEN Nil
    ELSE Changes(s + 1, to, IF logV[s].commit THEN s + 1 ELSE first,
                 [p \in Pages |-> IF p \in logV[s].pages THEN logV[s].tx ELSE acc[p]])

\* Drop the transactions a checkpoint at `start` covers.
DropBefore(log, start) == SelectSeq(log, LAMBDA t : t.end > start)

(*************************************************************************)
(* Processes.                                                            *)
(*************************************************************************)

Down == [pc |-> "down",
         \* the session's view: durable meta (decoded and raw), claim, log
         meta |-> DefaultMeta, metaRaw |-> Nil, claim |-> None, log |-> <<>>, next |-> 1,
         op |-> [none |-> TRUE],
         \* the owner cache, which survives sessions but not a crash
         cMeta |-> Unknown, cBase |-> None, cClaim |-> None, cCommitted |-> None,
         cPages |-> [p \in Pages |-> None]]

\* A ready session reads its log, then its cache, then the page key.
ReadPage(st, p) ==
    IF InSeq(st.log, p) THEN LatestIn(st.log, p)
    ELSE IF UseCache /\ st.cPages[p] # None THEN st.cPages[p]
    ELSE pageV[p]

View(st) == [p \in Pages |-> ReadPage(st, p)]

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

Resolve ==
    \E r \in pending, out \in Outcomes :
        LET k == r.key
            v == r.val
            vis == out \in {"ack", "lost", "visible"}
            dur == out \in {"ack", "lost"}
        IN /\ pending' = pending \ {r}
           /\ results' = [x \in DOMAIN results \cup {k} |-> IF x = k THEN out = "ack" ELSE results[x]]
           /\ logV' = IF k[1] = "log" /\ vis THEN Write(logV, k[2], v) ELSE logV
           /\ logD' = IF k[1] = "log" /\ dur THEN Write(logD, k[2], v) ELSE logD
           /\ pageV' = IF k[1] = "page" /\ vis THEN Write(pageV, k[2], v) ELSE pageV
           /\ pageD' = IF k[1] = "page" /\ dur THEN Write(pageD, k[2], v) ELSE pageD
           /\ metaV' = IF k[1] = "meta" /\ vis THEN v ELSE metaV
           /\ metaD' = IF k[1] = "meta" /\ dur THEN v ELSE metaD
           /\ UNCHANGED <<owner, ps, truth, unknown, txCount, crashes, violation>>

(*************************************************************************)
(* Open: read meta and replay; validate and refresh the cache; confirm  *)
(* on a weak backend what this owner did not write.                      *)
(*************************************************************************)

Open(pr) ==
    /\ owner = pr
    /\ pending = {}
    /\ ps[pr].pc \in {"down", "idle", "failed"}
    /\ LET st == ps[pr]
           m == metaV
           M == ReadMeta(m)
           known == UseCache /\ st.cMeta = m
           claim == IF known THEN st.cClaim ELSE None
           committed == IF known THEN st.cCommitted ELSE None
           since == IF UseCache /\ ~known /\ st.cBase # None /\ st.cBase <= M.start
                    THEN st.cBase ELSE None
           start == IF claim # None THEN claim.start ELSE M.start
           r == Replay(start, start, {}, <<>>)
           changed == IF since = None THEN Nil ELSE Changes(since, M.start, since, NoChange)
           cleared == UseCache /\ ~known /\ (since = None \/ changed = Nil)
           pages == IF cleared THEN NoChange
                    ELSE IF since # None
                    THEN [p \in Pages |-> IF changed[p] = None THEN st.cPages[p]
                                         ELSE IF st.cPages[p] # None THEN changed[p] ELSE None]
                    ELSE st.cPages
           tail == IF r.next > start /\ committed # r.next
                   THEN {[key |-> <<"log", r.next - 1>>, val |-> logV[r.next - 1]]} ELSE {}
           metaPut == IF m # Nil /\ ~known THEN {[key |-> <<"meta">>, val |-> m]} ELSE {}
           confirm == IF Weak THEN tail \cup metaPut ELSE {}
       IN /\ Issue(confirm)
          /\ ps' = [ps EXCEPT ![pr] =
                [st EXCEPT !.pc = "confirm",
                           !.meta = M, !.metaRaw = m, !.claim = claim,
                           !.log = r.txs, !.next = r.next,
                           !.op = [keys |-> Keys(confirm)],
                           !.cMeta = IF UseCache /\ ~known THEN Unknown ELSE st.cMeta,
                           !.cClaim = claim, !.cCommitted = committed,
                           !.cBase = IF cleared THEN None ELSE st.cBase,
                           !.cPages = pages]]
    /\ UNCHANGED <<store, owner, truth, unknown, txCount, crashes, violation>>

\* Confirmation settled: the session is ready, and what it reads must be the
\* history, possibly with the transaction whose outcome was unknown.
Opened(pr) ==
    /\ ps[pr].pc = "confirm"
    /\ Settled(ps[pr].op.keys)
    /\ results' = NoResults
    /\ LET st == ps[pr]
           view == View(st)
           with == IF unknown = None THEN truth ELSE Append(truth, unknown)
       IN IF ~Acked(st.op.keys)
          THEN /\ ps' = [ps EXCEPT ![pr] = [st EXCEPT !.pc = "failed"]]
               /\ UNCHANGED <<truth, unknown, violation>>
          ELSE /\ ps' = [ps EXCEPT ![pr] =
                    [st EXCEPT !.pc = "ready", !.op = [none |-> TRUE],
                               !.cMeta = st.metaRaw, !.cBase = st.meta.start,
                               !.cClaim = st.claim, !.cCommitted = st.next]]
               /\ IF view = Expected(truth) THEN UNCHANGED <<truth, violation>>
                  ELSE IF unknown # None /\ view = Expected(with)
                  THEN truth' = with /\ UNCHANGED violation
                  ELSE violation' = TRUE /\ UNCHANGED truth
               /\ unknown' = None
    /\ UNCHANGED <<store, pending, owner, txCount, crashes>>

(*************************************************************************)
(* Save: commit one transaction of one or two segments, with a           *)
(* checkpoint slice, garbage deletes and the claimed meta in the same    *)
(* round trip.                                                           *)
(*************************************************************************)

Save(pr) ==
    /\ owner = pr
    /\ pending = {}
    /\ ps[pr].pc = "ready"
    /\ txCount < MaxTx
    /\ LET st == ps[pr] IN
       \* one writer: nothing committed since this view was taken
       /\ ~UseCache \/ st.cCommitted = st.next
       \* the checkpoint may cover any prefix of whole transactions
       /\ \E changed \in (SUBSET Pages) \ {{}}, split \in BOOLEAN, k \in 0..Len(st.log) :
          LET tx == txCount + 1
              parts == IF split /\ Cardinality(changed) > 1 THEN 2 ELSE 1
              p1 == CHOOSE q \in changed : TRUE
              firstSeg == [first |-> st.next, commit |-> parts = 1, tx |-> tx,
                           pages |-> IF parts = 1 THEN changed ELSE {p1}]
              secondSeg == [first |-> st.next, commit |-> TRUE, tx |-> tx, pages |-> changed \ {p1}]
              covered == SubSeq(st.log, 1, k)
              ckPages == UNION {covered[i].pages : i \in DOMAIN covered}
              garbage == {s \in 1..MaxSeq : st.meta.del <= s /\ s < st.meta.start}
              collect == garbage # {} /\ (UseCache \/ k > 0)
              early == UseCache /\ st.claim # None /\ st.claim # st.meta
              maint == {[key |-> <<"page", p>>, val |-> LatestIn(st.log, p)] : p \in ckPages}
                       \cup (IF collect THEN {[key |-> <<"log", s>>, val |-> Nil] : s \in garbage} ELSE {})
                       \cup (IF early THEN {[key |-> <<"meta">>, val |-> st.claim]} ELSE {})
              firstReq == {[key |-> <<"log", st.next>>, val |-> firstSeg]}
          IN /\ st.next + parts - 1 <= MaxSeq
             /\ txCount' = tx
             /\ Issue(firstReq \cup maint)
             /\ ps' = [ps EXCEPT ![pr] = [st EXCEPT !.pc = "commit",
                 !.op = [tx |-> [tx |-> tx, pages |-> changed],
                         second |-> IF parts = 2 THEN {[key |-> <<"log", st.next + 1>>, val |-> secondSeg]} ELSE {},
                         firstKeys |-> Keys(firstReq),
                         commitKeys |-> IF parts = 1 THEN Keys(firstReq) ELSE {},
                         aborted |-> FALSE,
                         maintKeys |-> Keys(maint),
                         planEnd |-> IF k = 0 THEN None ELSE covered[k].end,
                         ckPages |-> ckPages,
                         images |-> [p \in Pages |-> LatestIn(st.log, p)],
                         collect |-> collect, garbageEnd |-> st.meta.start, early |-> early,
                         end |-> st.next + parts]]]
    /\ UNCHANGED <<store, owner, truth, unknown, crashes, violation>>

\* A two-part commit: the commit part goes out once the first is acknowledged.
CommitPart(pr) ==
    /\ ps[pr].pc = "commit"
    /\ ps[pr].op.commitKeys = {}
    /\ ~ps[pr].op.aborted
    /\ Settled(ps[pr].op.firstKeys)
    /\ LET op == ps[pr].op IN
       IF Acked(op.firstKeys)
       THEN /\ Issue(op.second)
            /\ ps' = [ps EXCEPT ![pr].op.commitKeys = Keys(op.second)]
       ELSE \* never committed; the save fails once maintenance settles
            /\ ps' = [ps EXCEPT ![pr].op.aborted = TRUE]
            /\ UNCHANGED <<pending, results>>
    /\ UNCHANGED <<store, owner, truth, unknown, txCount, crashes, violation>>

\* Commit and maintenance settled.
Committed(pr) ==
    /\ ps[pr].pc = "commit"
    /\ LET st == ps[pr]
           op == st.op
       IN
       /\ op.aborted \/ (op.commitKeys # {} /\ Settled(op.commitKeys))
       /\ Settled(op.maintKeys)
       /\ IF op.aborted \/ ~Acked(op.commitKeys)
          THEN \* an error: the outcome is unknown (a failed first part
               \* cannot have committed, but it is reported the same way)
               /\ unknown' = IF op.aborted THEN None ELSE op.tx
               /\ ps' = [ps EXCEPT ![pr] = [st EXCEPT !.pc = "failed", !.op = [none |-> TRUE],
                          !.cMeta = Unknown, !.cClaim = None, !.cCommitted = None]]
               /\ results' = NoResults
               /\ UNCHANGED <<truth, pending>>
          ELSE LET newLog == Append(st.log, [end |-> op.end, tx |-> op.tx.tx, pages |-> op.tx.pages])
                   durable == IF op.early THEN st.claim ELSE st.meta
                   durableRaw == IF op.early THEN st.claim ELSE st.metaRaw
                   del == IF op.collect /\ op.garbageEnd > durable.del THEN op.garbageEnd ELSE durable.del
                   target == [start |-> IF op.planEnd = None THEN durable.start ELSE op.planEnd,
                              del |-> del]
               IN
               /\ truth' = Append(truth, op.tx)
               /\ IF ~Acked(op.maintKeys)
                  THEN \* committed; checkpoint work failed: keep what was durable
                       /\ ps' = [ps EXCEPT ![pr] = [st EXCEPT !.pc = "ready", !.log = newLog,
                                 !.next = op.end, !.op = [none |-> TRUE],
                                 !.cMeta = Unknown, !.cClaim = None, !.cCommitted = op.end]]
                       /\ results' = NoResults
                       /\ UNCHANGED pending
                  ELSE IF target # durable /\ ~UseCache
                  THEN \* without a cache, meta in a second round trip
                       LET put == {[key |-> <<"meta">>, val |-> target]} IN
                       /\ Issue(put)
                       /\ ps' = [ps EXCEPT ![pr] = [st EXCEPT !.pc = "meta",
                                 !.log = newLog, !.next = op.end,
                                 !.meta = durable, !.metaRaw = durableRaw,
                                 !.op = [keys |-> Keys(put), target |-> target]]]
                  ELSE LET claim == IF target # durable THEN target ELSE None
                           base == IF claim # None THEN claim.start ELSE durable.start
                           \* this owner's checkpoint writes through its cache
                           pages == [p \in Pages |-> IF p \in op.ckPages THEN op.images[p] ELSE st.cPages[p]]
                       IN /\ ps' = [ps EXCEPT ![pr] = [st EXCEPT !.pc = "ready",
                                    !.log = DropBefore(newLog, base), !.next = op.end,
                                    !.meta = durable, !.metaRaw = durableRaw, !.claim = claim,
                                    !.op = [none |-> TRUE],
                                    !.cMeta = durableRaw, !.cBase = durable.start, !.cClaim = claim,
                                    !.cCommitted = op.end, !.cPages = pages]]
                          /\ results' = NoResults
                          /\ UNCHANGED pending
               /\ UNCHANGED unknown
    /\ UNCHANGED <<store, owner, txCount, crashes, violation>>

\* The second-round meta of a save without a cache.
MetaWritten(pr) ==
    /\ ps[pr].pc = "meta"
    /\ Settled(ps[pr].op.keys)
    /\ results' = NoResults
    /\ LET st == ps[pr]
           ok == Acked(st.op.keys)
       IN ps' = [ps EXCEPT ![pr] = [st EXCEPT !.pc = "ready",
                  !.meta = IF ok THEN st.op.target ELSE st.meta,
                  !.metaRaw = IF ok THEN st.op.target ELSE st.metaRaw,
                  !.log = IF ok THEN DropBefore(st.log, st.op.target.start) ELSE st.log,
                  !.op = [none |-> TRUE]]]
    /\ UNCHANGED <<store, pending, owner, truth, unknown, txCount, crashes, violation>>

(*************************************************************************)
(* Reads, session ends, handover and crashes.                            *)
(*************************************************************************)

\* A read of a page outside the log caches the page key's value.
Read(pr) ==
    /\ UseCache
    /\ owner = pr
    /\ ps[pr].pc = "ready"
    /\ \E p \in Pages :
        /\ ~InSeq(ps[pr].log, p)
        /\ ps[pr].cPages[p] = None
        /\ ps' = [ps EXCEPT ![pr].cPages[p] = pageV[p]]
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

\* The store loses what was visible but not durable; sessions end.
LoseVisible ==
    /\ Weak
    /\ crashes < MaxCrashes
    /\ Stable
    /\ logV # logD \/ pageV # pageD \/ metaV # metaD
    /\ logV' = logD /\ pageV' = pageD /\ metaV' = metaD
    /\ ps' = EndSessions
    /\ crashes' = crashes + 1
    /\ UNCHANGED <<logD, pageD, metaD, pending, results, owner, truth, unknown, txCount, violation>>

(*************************************************************************)
(* Specification and properties.                                         *)
(*************************************************************************)

Init ==
    /\ logV = [s \in 1..MaxSeq |-> Nil] /\ logD = logV
    /\ pageV = [p \in Pages |-> 0] /\ pageD = pageV
    /\ metaV = Nil /\ metaD = Nil
    /\ pending = {} /\ results = NoResults
    /\ owner \in Procs
    /\ ps = [pr \in Procs |-> Down]
    /\ truth = <<>> /\ unknown = None
    /\ txCount = 0 /\ crashes = 0 /\ violation = FALSE

Next ==
    \/ Resolve
    \/ \E pr \in Procs : Open(pr) \/ Opened(pr) \/ Save(pr) \/ CommitPart(pr)
                         \/ Committed(pr) \/ MetaWritten(pr) \/ Read(pr) \/ EndSession(pr)
    \/ Handover \/ CrashProc \/ LoseVisible

Spec == Init /\ [][Next]_vars

(* Every open reads the history: each acknowledged transaction, in order, *)
(* and the one with an unknown outcome or not; never part of one, never  *)
(* one that a previous open decided was not committed.                    *)
OpensReadTheHistory == ~violation

(* A ready session reads the history at every moment, through its log,    *)
(* its cache and the page keys: no checkpoint exposes a stale page.       *)
ReadyReadsTheHistory ==
    \A pr \in Procs : (ps[pr].pc = "ready" /\ owner = pr) => View(ps[pr]) = Expected(truth)
=============================================================================
