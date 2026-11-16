Judge the approved intent independently on the current head. You did not build it.

Open every approved reference and every built image yourself. Compare each visual
requirement at the same viewport, theme and state. Verify behaviour, wording and
data requirements too. The attached ledger is authoritative: do not substitute the
build's summary or reduced acceptance criteria. Every unexplained gap fails. A
recorded operator authorization can permit a deviation; an agent cannot approve
its own deviation. Report an unreadable or missing image as a failure.

Do not edit tracked files. Submit the ordinary verdict plus this exact structure:

```json
{"acceptance":{"head":"<git rev-parse HEAD>","tree":"<git rev-parse HEAD^{tree}>",
 "intent":{"id":"<id>","version":1,"requirements":[]},
 "requirements":[{"id":"R1","verdict":"pass","reason":"what was checked against the reference"}]}}
```

Copy the intent binding exactly, including its requirement subset. Every selected
requirement needs one judgment. Use verdict `fail` for any gap or unapproved
deviation. Archon renders and publishes your reference | built acceptance page.

For a validation-only run with no build evidence on record, also submit
`requirement_results` with your observations and reference | built image pairs
using the attached build-result contract. You cannot authorize a deviation.
