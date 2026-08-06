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

  return failures.length === 0
    ? { pass: true, score: 1, reason: `Matches golden set (verdict=${verdict})` }
    : { pass: false, score: 0, reason: failures.join("; ") };
};
