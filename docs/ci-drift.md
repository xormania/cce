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

Jev answers independent typed questions about proof, guarantees, witnesses,
deferrals and resolutions. These answers are advisory. They never change test
outcomes or convert a deterministic failure to success. The summary shows the
classification and the relevant public file where one is available. Private
subjects use opaque references.

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

Data is fetched at one resolved commit. A candidate-matching, clean mutation
receipt is compared per run; raw aggregate `caught` cannot override the accepted
disposition. The adapter reads at most twelve current mutation receipts and
reports bounded/incomplete input rather than silently claiming all were checked.

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

Deferred packet fingerprints remain visible and get priority on the next
evaluation of that source context. Source jobs, explicit rechecks and the bounded
scan provide further evaluations. A changed-file comparison is limited to forty
files; omitted files and unavailable logs are reported as coverage limitations.
Truncated excerpts are marked. Large allocations, crash campaigns and mutation
commands appearing in logs are never executed by this observer.

## Local validation

Run `python3 -m unittest discover -s tests/ci -v` from a plain checkout. It requires
only Python 3.11 or newer and no credentials or network. The tests include paired
defect/healthy replays, actual isolated CLI execution, provider rejection and
non-execution, private marker handling, bounded artifacts, deduplication and
candidate check lifecycle. Synthetic provider replies validate integration behavior;
they are not measurements of the model's diagnostic accuracy.
