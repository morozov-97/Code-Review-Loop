// report.md는 자유 텍스트라 promptfoo 스킬의 "구조화 필드 비교" 패턴을 그대로 못 쓴다 —
// 대신 verdict 한 줄과 포함/미포함 키워드로 판정한다. finding claim/evidence 문구 자체는
// LLM마다 표현이 달라지므로(비결정적) golden으로 안 쓰고, "그 취약점이 finding으로
// 잡혔는지"만 핵심 키워드(함수명 등) 포함 여부로 느슨하게 확인한다.
module.exports = (output, context) => {
  const report = String(output ?? "");
  const metadata = context?.test?.metadata ?? context?.testCase?.metadata ?? {};
  const failures = [];

  const verdictMatch = report.match(/Verdict:\s*([A-Z_]+)/);
  const verdict = verdictMatch ? verdictMatch[1] : null;

  const expectVerdictIn = metadata.expectVerdictIn;
  if (expectVerdictIn) {
    if (!verdict) {
      failures.push("report.md에서 Verdict 줄을 못 찾음");
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
    ? { pass: true, score: 1, reason: `골든셋과 일치 (verdict=${verdict})` }
    : { pass: false, score: 0, reason: failures.join("; ") };
};
