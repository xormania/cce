# CI contract checks and drift diagnosis

CCE evaluates execution evidence and changed code, tests and documentation even
when the Rust suite passes. `drift-input` runs the offline tooling tests on pushes,
including Markdown-only changes. `Jev drift analysis` uses trusted default-branch
code to read candidate files and completed job logs as data. It never imports or
executes candidate tooling in the credential-bearing job.

## Results

`CCE drift contracts` is attached to the evaluated commit. It starts in progress
before evidence collection and fails on applicable deterministic contradictions:

- A required step failed or did not execute.
- A successful test step has no demonstrated nonzero test execution.
- An explicit complete claim contradicts its applicable mapped cases.
- A claimed semantic mutation catch has no assertion failure, or conflicts with
  its accepted disposition. A process abort is not an assertion catch.

Missing logs or still-running source jobs leave validation incomplete, not passed.
A collection error replaces the candidate's previous success with failure or an
in-progress result. The publisher rechecks the branch/PR head and check ownership;
an older consumer cannot clear a newer candidate or overwrite a newer analysis.
For a PR, publication also rechecks the collected base revision. A changed base
invalidates the earlier evaluation even when the branch head has not moved.

Test counts are attributed to the required step's trusted command and execution
interval. Missing or ambiguous step output leaves evidence incomplete. A positive
unittest discovery count consisting entirely of skipped cases does not prove test
execution; a different step's count cannot supply the missing witness.

Producer names and printed test counts alone are insufficient. The collector
compares producer workflow contents with the trusted checkout before accepting
execution. PR producers also identify the checked-out merge commit and current
base. For a pull request, the trusted leak scan's `scan` check is evaluated
alongside tooling and Rust tests (see `docs/ci-leak-scan.md`). A scan that
reports it could not run cannot certify execution.

For a Markdown follow-up to a mixed PR, an earlier Rust push run can supply evidence
only when a bounded ancestor comparison proves every intervening changed and
renamed path matches the trusted producer's Markdown filter. Its actual revision,
run, attempt and workflow hash remain attached. An applicable newer failure is
preserved. An unfiltered PR producer still requires its current merge-candidate run.

Changed Python provisioning can be validated automatically when it preserves the
test contract. The supported form adds the trusted, commit-pinned `setup-uv`
action with an exact numeric uv version and disabled caching, then installs an
exact Python version and runs the unchanged unittest command through
`uv run --no-project --python` with that same version. The setup step must be
immediately before the test step. Removing that setup and interpreter wrapper
must leave the entire workflow identical to the trusted definition. The same
comparison supports later runtime version updates and a return to direct Python.
Changes to discovery arguments, conditions, environment, triggers, permissions,
other steps or action pins do not qualify. This is a bounded compatibility rule,
not a general YAML or shell equivalence checker.

Automatic validation records `producer_acceptance.policy = "python-runtime-v1"`
and `producer_definition_auto_validated_drift_input`. It proves compatibility
only: the actual candidate definition, full command, source revision and test
output must still match the execution evidence. Failed, zero-test or all-skipped
runs remain unsuccessful; candidate assertions cannot certify execution.

For changes outside that supported form, acceptance still requires the repository
owner's latest approving review of the pull request's exact head commit. A later
push needs a new review, and an approval from another account, of an older head,
or followed by a change request does not count. Every review page is read, and an
unreadable or unbounded review list fails closed. Owner-accepted rows record the
review, and the coverage line names each such producer
(`producer_definition_owner_accepted_<producer>`). After the
change merges, the default branch's copy is the trusted definition again.

A workflow awaiting acceptance produces `workflow_definition_awaiting_acceptance`.
Its normalized policy row has `definition_status = "awaiting_acceptance"` and
`executed = null`: the analysis has not verified execution under an accepted
definition. It does not assert that tests failed or never ran. This policy finding
keeps the contract result unsuccessful until acceptance can be established.
Actual execution findings continue to use their own codes.

Execution and acceptance findings are reported separately, each with its remedy:

| Code | Meaning | Remedy |
|---|---|---|
| `required_check_failed` | The producer ran and the step failed. | Fix the failure. |
| `required_execution_unavailable` | No applicable run exists yet. | Wait for or start the run. |
| `producer_merge_identity_unavailable` | The run happened, but its merge commit cannot be identified (for example the PR conflicts with its base). | Update the branch from its base. |
| `workflow_definition_awaiting_acceptance` | A workflow definition has not satisfied the acceptance policy; test execution remains unverified by this analysis. | Obtain owner review of this head, or drop the workflow change; align the actual run’s workflow with the accepted definition if the merge commit differs. |
| `producer_definition_unaccepted` | Collection reason retained for the workflow acceptance finding. | Inspect the workflow acceptance finding. |
| `unsupported_producer_definition` | The steps cannot be recognized. | Keep the named direct-command steps. |

Jev answers independent typed questions about proof, guarantees, witnesses,
deferrals and resolutions. These answers are advisory. They never change test
outcomes or convert a deterministic failure to success. The summary shows the
classification and the relevant public file where one is available. Private
subjects use opaque references.

Packets join affected before/after functions or document sections with linked
contracts and named proof. Hunk locations select the context; assertions near the
end of long tests are retained. Excerpts identify immutable revisions, line ranges,
content hashes and omissions. Unavailable or ambiguous proof is reported explicitly.

Only candidate-applicable evidence can fail the candidate. Historical notes,
honestly partial work and unavailable local source are not automatically defects.
An explicitly accepted equivalent/redundant mutation is not counted as semantic
detection. The collector uses exact recorded source identity for data-backed
blocking claims; it does not infer equivalence from a model answer or a new SHA.

## Configuration and activation

Add repository secret `TYPESAFE_API_KEY`. The provider step uses the pinned
`jev-1.14.0` model with a 15-second timeout, one request per packet and at most four
packets per run. There are no automatic provider retries. Responses are bounded
and validated; duplicate/invented supporting references are rejected.

The trusted `workflow_run` consumer becomes active after this workflow and its
scripts are on the default branch. Before that, PR checks prove the candidate
tooling offline; they do not prove deployed diagnosis or live provider access.

After activation:

1. Push a small code/test or Markdown change and inspect `Jev drift analysis`.
2. Confirm its trusted revision and source candidate, nonzero provider request
   count, observed model, and diagnosed/abstained/rejected outcome in `drift-state`.
3. Enable the **`CCE drift contracts`** check in the repository's required-check
   rules, with GitHub Actions as its expected source. This settings change is
   necessary for merge enforcement; merely adding a workflow does not enable it.

Until required-check configuration is enabled, the check reports failures but is
not itself a merge restriction. Existing source checks retain their own verdicts.

Manual dispatch accepts `head_sha` for an existing candidate. `force` explicitly
allows repeating an earlier reserved provider request. Dispatch must use the
default branch's trusted workflow. A twice-hourly bounded scan rechecks an open
same-repository PR (rotating among the first twenty), or the default branch when
there are no such PRs. This can pick up data-only corrections without a code edit.
The scan is not a complete inventory of more than twenty concurrent PRs; dispatch
an omitted candidate explicitly. Source-CI events continue to analyze each push.

## Optional versioned data input

Public change/log analysis works with the model key alone. A versioned private
data source can additionally supply existing requirement maps, status summaries,
mutation results and bounded evidence records. No workflow or executable tooling
is needed in the data repository.

Configure these repository secrets for that input:

| Secret | Value |
| --- | --- |
| `DRIFT_DATA_REPOSITORY` | Repository in `owner/name` form. |
| `DRIFT_DATA_TOKEN` | Read-only contents access to that repository. |
| `DRIFT_DATA_FILES` | JSON object naming `requirements` (TOML), `summary` (Markdown), and optionally `records` (up to eight file paths). |

For example, the file-selector shape is
`{"requirements":"records/cases.toml","summary":"records/status.md","records":["records/recent.log"]}`.
These are example paths, not files that agents need to create for each change.
The adapter consumes existing data: `[[cases]]` entries with `id`, `slice` and
`status`; optional `[[mutations]]` entries with accepted `disposition`, `source_head`
and `evidence`; and a summary table with a `verified-at` candidate stamp. An explicit
`N/N green` summary is compared with the mapped cases. Free-form text remains a
semantic question rather than a brittle deterministic parser rule.

The adapter follows existing requirement acceptance lists, case test names, and
bounded relative links in existing records. It joins private claim/decision context
to the named public assertions without publishing private locations. Recorded
claims identify the pinned data revision; public proof identifies the code revision.
Mapped proof is collected in rotating batches of eight cases with bounded reads;
deferred recipes can request their original case directly, regardless of rotation.
The configured data inventory supports up to forty summary groups; exceeding this
bound reports incomplete coverage and fails when that input is explicitly required.

Data is fetched at one resolved commit. A candidate-matching, clean mutation
receipt is compared per run; raw aggregate `caught` cannot override the accepted
disposition. The adapter reads at most twelve current mutation receipts and
reports bounded/incomplete input rather than silently claiming all were checked.

Mapped cases and per-group status evaluation share a 1,000-case bound; the
separate normalized-evidence inventory remains bounded at 256 rows. An oversized
optional input reports a coverage limitation; requiring that input makes the
missing coverage unsuccessful. No late case is silently dropped.

Mutation receipts retain their raw exit codes. Negative subprocess returns and
Linux shell signal exits (129 through 192) cannot certify an assertion catch.
An optional `exit_convention = "subprocess"` identifies positive application
exit codes, while `"shell"` identifies shell encoding. Without that field,
signal-looking codes are treated conservatively. An explicit `aborted = true`
always takes precedence; normal assertion exits such as 101 remain usable.

Set repository variable `DRIFT_DATA_REQUIRED=true` only when this data source is
configured and required for candidate acceptance. Missing credentials, invalid
data or an unproven data/candidate association then fails required validation.
Without this variable, the public result explicitly identifies unavailable data
as a coverage limitation. The model key does not grant GitHub data-repository
access; the data token has its own scope.

## Evidence, privacy and limits

The public renderer emits only fixed classification codes, public source
references, numeric execution metadata and opaque fingerprints. Private excerpts,
paths, identifiers and raw provider responses are never uploaded. Full private
diagnostic replay is consequently unavailable from the public artifacts; existing
versioned originals remain in their data source. Input and response hashes identify
the analysis without disclosing those bytes. Neither credentials nor exception
strings are printed.

`drift-state` retains public results for fourteen days. `drift-reservation` is
uploaded before provider execution; a cancelled or failed attempt remains reserved
until explicit replay or history expiration. The collector inspects a bounded
history window and reports when no reusable baseline exists. It does not promise
permanent suppression. Changed evidence or model/tooling configuration reopens
analysis; unrelated candidate identity alone does not change packet fingerprints.

Completed provider results are checkpointed individually by atomic replacement.
If a later request is interrupted, publication preserves the completed prefix and
reports a partial provider stage. It neither repeats unfinished requests nor
turns an advisory-provider interruption into a deterministic test failure.
Re-entering a partial phase requires a new explicit replay; a completed phase is
idempotent. These checkpoints protect against step interruption, not loss of the
entire runner or an artifact upload that never occurs.

Deferred packet fingerprints remain visible and get priority on the next
evaluation of that source context. Source jobs, explicit rechecks and the bounded
scan provide further evaluations. A changed-file comparison is limited to forty
files; omitted files and unavailable logs are reported as coverage limitations.
Truncated excerpts are marked. Large allocations, crash campaigns and mutation
commands appearing in logs are never executed by this observer.

Deferred, reserved, unavailable and tracked relationships also retain allowlisted
reconstruction recipes. These contain original public source identities and opaque
data selectors, without private paths or excerpts. They survive unrelated pushes
and branch integration. Reconstruction refetches the pinned original context; a
historical observation is labelled as such and cannot fail a newer candidate.
Unavailable originals stay unfinished. At most two original contexts are rebuilt
per run, with unfinished work ahead of tracked results and rotation to avoid
starvation. The retained inventory is bounded at two hundred recipes; overflow is
reported as continuing coverage loss.

A healthy public change retires an older finding only when correction metadata
ties it to the affected lines in the same uniquely identified function or
document section. Another change in the same file or function is insufficient.
The metadata contains bounded hashes, not source excerpts. A real repair or
restoration can close the finding; ambiguous identity stays unfinished. Legacy
recipes can acquire this metadata by reconstructing their pinned original input.

Tooling changes invalidate cached diagnoses while preserving validated unfinished
recipes and the newest published queue. Retention overflow is a historical lower
bound, not a current outstanding-work count; repeatedly seeing the same omitted
inventory does not increase it. Older incompatible counters retain a coverage
loss indicator without claiming their accumulated number is accurate.

Reservation artifacts preserve these recipes before a provider call, including if
the job is cancelled before publication. Missing credentials do not reserve an
unattempted request. The latest published queue supersedes older snapshots so a
resolved relationship cannot be resurrected by an old artifact. Unchanged semantic
content reuses its result even when source revision annotations change.

## Local validation

Run `python3 -m unittest discover -s tests/ci -v` from a plain checkout. It requires
only Python 3.11 or newer and no credentials or network. The tests include paired
defect/healthy replays, actual isolated CLI execution, provider rejection and
non-execution, private marker handling, bounded artifacts, deduplication and
candidate check lifecycle. Synthetic provider replies validate integration behavior;
they are not measurements of the model's diagnostic accuracy.

## Owner decisions and claims (optional data)

With versioned data configured (`DRIFT_DATA_REPOSITORY`, `DRIFT_DATA_TOKEN`,
`DRIFT_DATA_FILES`), the observer also reads a decision ledger and selected status
documents at one pinned data revision. Configure them in `DRIFT_DATA_FILES` as
`"ledger": "<path>"` and `"documents": ["<path>", ...]` (at most 24). Use a token
that can only read that repository's contents.

The ledger links each question to the answers that settle it, the work they
authorize and agent proposals. `tools/ci/drift_ledger.py` resolves it
deterministically: an answer is replaced only by one that names it in
`supersedes` (never by date), two unordered answers are a conflict, a proposal is
never an answer, a proposal that would impose a restriction a deferral excludes
is flagged, and an unanswered question must name the operation it blocks. The
same module prints the current view for agents (`drift_ledger.py view`).

Ledger format 2 records who recorded each answer (`recorded_by`). An owner answer
also carries the owner's words (`quote`), or says why only a transcription
survives (`quote_unavailable`). Work closes as `done` (with evidence),
`withdrawn` (with a reason) or `superseded` (naming its successor). Open work
under a replaced answer is reported, so it is never left behind silently.

Every entry has a revision `rev`. Writers publish through
`drift_ledger.py update --base READ --current LATEST --proposed EDITED --out FILE`.
It applies the writer's changes onto the latest ledger by entry identity and
keeps other writers' unrelated changes. It refuses:

- a change to an entry that someone else changed since it was read (stale
  revision, even if the value was changed back);
- a change that does not advance `rev` by one;
- an edited answer (record a new answer that supersedes it instead);
- a removed entry;
- an identity reused with different content.

Re-applying changes that are already published reports `unchanged`, so a retry
after a lost response has no duplicate effect.

### Scoped context for agent sessions

`tools/ci/ledger_context.py` reads the same ledger for one session's current work
instead of printing every record:

- `brief` renders the records that govern a scope (a JSON descriptor with an
  objective, the work items in hand and who set it): current answers to questions
  marked `applies = "always"`, each work item with the current answer to its
  question and that answer's `forbids`, and every missing decision, conflict and
  finding. Without a scope it covers all open work and says so. Other current
  records are only named (identity and question title): the budget is a
  ceiling, and loading one in full takes a relevance judgment.
  The whole output, including the caller's header, stays within a character and
  byte budget. Text is never cut: governing records that do not fit are named with
  the read that returns them.
- `read ID...` (or `read inventory`) returns the exact stored fields, written as
  TOML, in pages. A cursor is bound to the ledger snapshot and the request, so a
  page from a changed ledger is refused instead of mixed.
- `changes` compares a held checkpoint with the ledger and reports added, changed,
  superseded, unchanged, missing and unknown records separately. A record whose
  status changed (for example an answer that was superseded) counts as changed.

The summary in each line is the recorder's wording. The brief marks whether the
owner's own words are stored with it (`quoted`) or only a paraphrase survives;
`read` shows the quotation as stored. An owner quotation may be longer than other
text fields, so a long message is kept whole.

The writer keeps everything the reader accepts:
- every TOML value, including booleans, floats (also `inf` and `nan`), dates and
  times, inline tables, arrays and quoted keys;
- top-level data outside the entries;
- full-line comments.

It reads back what it wrote and refuses (`serialization_mismatch`) if anything
differs. The output file is replaced in one step. A comment after a value on the
same line cannot be kept in place, so an update containing one is refused
(`inline_comment_not_kept`) rather than dropping it.

Each finding (for example a document line presenting a settled question as still
waiting for the owner) becomes one private packet. It contains only:

- the claim's surrounding document lines (not the document's path);
- the applicable answer's fields: identity, question, authority, outcome, date
  given, summary, excluded restrictions, superseded answers and source path;
- any answer it superseded (identity, questions, summary, superseded by);
- for a proposal or work finding, that record's identity, questions, answer,
  summary, state and restrictions;
- the finding itself: code, question, answer, proposal or work identity and line.

Packet identities are keyed with the ledger's content, so a public reference
cannot confirm a guessed finding. Jev's answer is advisory; findings never fail a
candidate. Public output keeps only opaque codes such as `decision_record_findings`.

Without decision data the summary states that private owner-decision consistency
was not checked. A data-only correction is evaluated by the next run (or a
dispatched run with `force`), which reads the data repository's current revision.
