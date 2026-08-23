/// Repo names out of GET /brain — `{ generated_at, repos: [{ name, ... }] }`.
export function repoNames(brain) {
  return (brain?.repos || []).map((r) => r?.name).filter(Boolean).sort();
}
