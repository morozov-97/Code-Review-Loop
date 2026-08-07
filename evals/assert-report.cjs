// report.md is free-form text, so the promptfoo skill's "structured field comparison" pattern
// doesn't directly apply — instead we judge it by a single verdict line plus keyword
// inclusion/exclusion. The finding claim/evidence wording itself varies by LLM (non-deterministic),
// so it isn't used as golden data; we only loosely check "whether that vulnerability was caught
// as a finding" via the presence of key keywords (function names, etc.).
module.exports = (output, context) => {
  const report = String(output ?? "");
  const metadata = context?.test?.metadata ?? context?.testCase?.metadata ?? {};
  const failures = [];

  const verdictMatch = report.match(/Verdict:\s*([A-Z_]+)/);
  const verdict = verdictMatch ? verdictMatch[1] : null;

  const expectVerdictIn = metadata.expectVerdictIn;
  if (expectVerdictIn) {
    if (!verdict) {
      failures.push("Could not find a Verdict line in report.md");
    } else if (!expectVerdictIn.includes(verdict)) {
      failures.push(`verdict: expected one of ${JSON.stringify(expectVerdictIn)}, got ${verdict}`);
    }
  }

  for (const needle of metadata.expectContains ?? []) {
    if (!report.includes(needle)) {
      failures.push(`report.md should contain ${JSON.stringify(needle)} but doesn't`);
    }
  }

  for (const needle of metadata.expectNotContains ?? []) {
    if (report.includes(needle)) {
      failures.push(`report.md should NOT contain ${JSON.stringify(needle)} but does`);
    }
  }

  // #176: run-codereview.sh/run-codereview-default-rounds.sh append a
  // "<!-- MANIFEST_PROVIDER_CALLS: N -->" marker built from manifest.json's usage.calls.
  // Previously nothing here asserted on token/call counts at all, so a change that doubled the
  // number of provider calls for a case would still pass cleanly as long as the verdict came
  // out right. This is a loose regression guard (generous thresholds meant to catch a gross
  // blowup), not a tight cost budget — there's no real historical data in this repo to calibrate
  // a tight one against.
  const callsMatch = report.match(/MANIFEST_PROVIDER_CALLS:\s*(\d+)/);
  const providerCalls = callsMatch ? Number(callsMatch[1]) : null;
  if (metadata.expectMaxProviderCalls != null) {
    if (providerCalls === null) {
      failures.push("Could not find a MANIFEST_PROVIDER_CALLS marker in output");
    } else if (providerCalls > metadata.expectMaxProviderCalls) {
      failures.push(
        `provider calls: expected <= ${metadata.expectMaxProviderCalls}, got ${providerCalls}`
      );
    }
  }

  return failures.length === 0
    ? { pass: true, score: 1, reason: `Matches golden set (verdict=${verdict})` }
    : { pass: false, score: 0, reason: failures.join("; ") };
};
