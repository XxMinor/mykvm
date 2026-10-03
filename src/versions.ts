/** Semver precedence ("0.9.13-beta.100" < "0.9.13-beta.100.1" < "0.9.13"). */
export function compareVersions(left: string, right: string): number {
  const parse = (version: string) => {
    const [core, pre] = version.trim().split("-", 2);
    return { core: core.split(".").map(Number), pre: pre ? pre.split(".") : [] };
  };
  const a = parse(left);
  const b = parse(right);
  for (let i = 0; i < 3; i += 1) {
    const diff = (a.core[i] || 0) - (b.core[i] || 0);
    if (diff !== 0) return Math.sign(diff);
  }
  // A release outranks any of its pre-releases.
  if (!a.pre.length || !b.pre.length) return Math.sign(b.pre.length - a.pre.length);
  for (let i = 0; i < Math.max(a.pre.length, b.pre.length); i += 1) {
    if (a.pre[i] === undefined) return -1;
    if (b.pre[i] === undefined) return 1;
    const [x, y] = [a.pre[i], b.pre[i]];
    const numeric = /^\d+$/.test(x) && /^\d+$/.test(y);
    const diff = numeric ? Number(x) - Number(y) : x.localeCompare(y);
    if (diff !== 0) return Math.sign(diff);
  }
  return 0;
}
