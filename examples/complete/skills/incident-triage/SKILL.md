---
name: incident-triage
description: Use when asked to triage, investigate, or diagnose a Kubernetes incident, outage, or unhealthy workload. Gives the step-by-step order of investigation and the format for the findings.
---
# Incident triage

Follow these steps in order. Do not skip ahead to a fix.

## 1. Establish scope

- Identify the namespace and workload named in the request. If none is
  given, list namespaces and ask which one before continuing.
- Record the time the problem was first noticed.

## 2. Check workload state

- List pods for the workload and note any that are not `Running`/`Ready`.
- For each unhealthy pod, note restart count, last termination reason, and
  the age of the current container.

## 3. Read recent events

- List events for the namespace, most recent first.
- Look for: `OOMKilled`, `CrashLoopBackOff`, `FailedScheduling`,
  `ImagePullBackOff`, probe failures, node pressure.

## 4. Read logs — narrowly

- Fetch logs only for the unhealthy pods, only the tail (last 200 lines).
- Search for stack traces, connection refused/timeouts, and the first
  error after the last successful startup line.

## 5. Report

Use exactly this structure:

```
Impact:      <what is broken, for whom>
Since:       <timestamp, from events or first error>
Cause:       <best current hypothesis, with the evidence line that supports it>
Evidence:    <pod names, event reasons, log excerpts — quoted, not paraphrased>
Next step:   <one action, read-only unless the user asked for a fix>
```

If the evidence does not support a single cause, say so and list the two
most likely candidates with what would distinguish them.
