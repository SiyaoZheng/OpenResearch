---
name: orx-reports
description: "Write reports to the project's designated destination and organize artifact files. Use before creating or organizing artifacts, including reports, summaries, and figures."
---

An artifact is a project-wide deliverable that a human researcher will read and analyze. Figures, HTML pages, and other files go in the artifacts directory. Projects without a report database also save reports there.

In a project with a Notion report database, write independent reports directly with
`ntn`. Use the project playbook's workspace and fields. In an older experiment
checkout, consult the current project guide on its baseline branch. Query the report's stable
artifact-relative source key with `ntn datasources query`. For one matching report,
read its complete v2 Markdown and patch only the report above the team-notes heading
with `update_content` on `v1/pages/<id>/markdown`. Preserve its notes and attachments.
For a new key, send JSON on stdin to `ntn api v1/pages -X POST -d @-` with
`parent.data_source_id`, `markdown_version` set to `v2`, the Markdown body, and
properties for title, source key, category, experiment time and time basis.
Resolve duplicate or wrong-category matches before writing.

Keep the artifacts directory clean; avoid model checkpoints, rollouts, activations, etc. that are not project-wide that will bloat the artifacts directory.

Use a descriptive filename for each output. For example:

```text
<artifacts-dir>/
  learning-rate-study/
    loss-curves.html
    final-accuracy.svg
```

Read the `orx-figures` module before writing any figure; a default
matplotlib plot does not meet the bar the reports are held to.

Load `orx-results` when the report makes claims derived from run results.

Hand off a Notion report with its page URL. In the page, use attachments or URLs
the team can open. Use relative links within file-based reports, for example
`![Final accuracy](final-accuracy.svg)` from `learning-rate-study.md`.
In the chat handoff, link every finished output using the session playbook's
evidence-and-links contract, including the full path, for example
`<file path="artifacts/learning-rate-study/final-accuracy.svg" />`.
