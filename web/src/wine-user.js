// The Windows user a Wine page runs as: gex, the user its prefix's profile is
// made for (tools/wine-wasm/page-prefix.sh), unless the caller names one.
// Without USER the page has no user name (no /etc/passwd), and Wine makes a
// profile with an empty name (C:\users\AppData).
export const PAGE_USER = 'gex';

export function pageUser(env) {
  for (const k of ['USER', 'LOGNAME'])
    if (!env.some((e) => e.startsWith(`${k}=`))) env.push(`${k}=${PAGE_USER}`);
  return env;
}
