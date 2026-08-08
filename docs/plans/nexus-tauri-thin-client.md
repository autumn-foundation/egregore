# Nexus in a Tauri Shell (Desktop + Mobile) — Thin-Client Planning Dossier

Revision 2 — after a two-angle Opus review (security/trust-model; TDD/accuracy),
46 findings folded in.

Status: **planning deliverable only** — produced in a session bound to
`autumn-foundation/egregore`, which cannot attach the private Nexus repository
(`wheelhorsedev/nexus`; cross-tier attach refused, and its issue tracker is
likewise unreachable from here). Implementation, the red/green/refactor test
work, the multi-angle code review of the *code*, and the real issue's
AC-evidence table must be executed in a session started **from
`wheelhorsedev/nexus`**. This dossier is written to be carried into that
session and executed as-is.

Ground truth used (read in full, then re-verified line-by-line by the review
agents):

- `autumn-foundation/autumn` `docs/guide/tauri-mobile-thin-client.md`
  (the guide the task names) and `docs/guide/tauri.md` (desktop sidecar guide).
- `autumn-cli/src/generate/tauri.rs` — `plan_tauri_thin_client`,
  `validate_remote_url`, `ensure_no_opposite_mode_scaffold`,
  `THIN_CLIENT_MARKERS` / `DESKTOP_MARKERS`, rendered templates, and
  `emit.rs::Plan::revert` (destroy semantics).
- `autumn/src/session.rs`, `autumn/src/security/config.rs`,
  `autumn/src/config.rs` (prod-profile smart defaults) — so no proposed test
  asserts something the framework already guarantees.

## 0. The one architecture decision this plan encodes

**Both desktop and mobile use the same thin-client scaffold**:
`autumn generate tauri --remote-url https://<nexus-prod-domain>` — one
`src-tauri/` sub-project whose webview loads the cloud-hosted Nexus server over
HTTPS. There is no sidecar, no bundled database, and no staging step on any
target.

Honest scope of the desktop claim: the sources document that the thin-client
crate *compiles and smoke-tests* on desktop (notification and store plugins
build on every target; biometric is target-gated in **three** places — the
Cargo dependency under `cfg(any(target_os = "android", target_os = "ios"))`,
the `#[cfg(mobile)]` registration in `lib.rs`, and the `platforms`-restricted
capability file). **Shipping it as a desktop product is this plan's own
extension, not a documented mode**, and carries four desktop-specific deltas
this plan owns explicitly:

1. Window polish is a hand edit — the generated thin-client `lib.rs` sets only
   `.title()`, no `.inner_size()` (unlike the sidecar scaffold's 1200×800).
2. Credential-file permissions: the thin-client scaffold has none of the
   sidecar scaffold's `0o700`/`0o600` hardening, so any `tauri-plugin-store`
   file lands with the process umask on multi-user desktops (→ T2.3).
3. No updater: the capability grant (the `remote.urls` origin) is compiled
   into the shipped binary. With no updater there is **no revocation path** —
   if the prod domain is ever lost or taken over, every installed desktop
   shell hands device-API access to whoever controls that DNS name (→ §2 R19,
   §7 decision required).
4. A desktop store/no-store decision: simplest safe answer is cookie-only on
   desktop (no persisted token file), reserving the token handoff for mobile.

The generator **refuses to mix desktop-sidecar and thin-client modes in one
tree** (`ensure_no_opposite_mode_scaffold`, enforced even under `--force`), so
"sidecar on desktop + thin client on mobile" would demand two trees and is
deliberately NOT this plan. Caveat found in review: the *third* mode,
`autumn generate tauri-mobile` (in-process backend), emits **no unique marker
files**, so the guard cannot block a thin-client scaffold over it — ruling
that out is a review rule, not a tool guarantee. If a self-contained desktop
app is ever required, that is a mode switch:
`autumn destroy tauri --remote-url <exact-original-URL> --force && autumn
generate tauri` — destroy must receive the same URL the scaffold was generated
with, and requires `--force` once the identifier/icons/window polish have been
hand-edited, because destroy refuses on diverged content.

## 1. Planning — Brainstorming (divergent options)

1. **A single thin-client scaffold for all targets** (chosen; rest of this
   dossier). One deployment serves browsers, PWA, desktop shell, and mobile
   shells; a server fix updates every installed app instantly.
2. **Desktop sidecar + mobile thin client.** Full offline desktop, but two
   architectures, two auth stories, and mode-guarded mutual exclusion in one
   tree. Rejected for now.
3. **`frontendDist` as a remote URL** (config-only form). Rejected: the
   generator deliberately avoids it — `tauri dev` has a known bug with
   URL-form `frontendDist` (tauri-apps/tauri#12333) — and the Rust-side
   `WebviewUrl::External` builder keeps the URL next to the plugin
   registration.
4. **PWA only, no Tauri.** Zero store presence, no native plugins. Rejected as
   the sole path; note `autumn generate pwa` composes with Tauri (its icon is
   auto-reused by the Tauri scaffold) and is this plan's preferred vehicle for
   the offline page (§5 Phase 5 option b).
5. **In-process backend modes.** Options B and C of the autumn roadmap
   (#1506 A = this plan; #1507 B; #1508 C) are **already implemented** as
   `autumn generate tauri-mobile` and `autumn generate tauri-mobile
   --offline-sync`, with their own guides. They are rejected here as an
   architecture *choice* (online-first Nexus, one deployment), not because
   they are unavailable; migrating later is a full mode switch (destroy +
   regenerate + app-crate `lib.rs` extraction), not an increment.
6. **Value-add ideas folded in**: biometric-gated token release *as a local
   unattended-device control* (see §2 R17 for what it does NOT provide);
   offline retry view; a shared `window.__TAURI__` detection JS module;
   store-plugin draft persistence for flaky mobile networks.

## 2. Planning — Reverse brainstorming ("how would we guarantee failure?")

Each failure recipe becomes a mitigation and, where testable, a test in §5.

| # | Guaranteed-failure move | Mitigation (→ test) |
|---|---|---|
| R1 | Widen `remote.urls` beyond the one prod origin | Capability files carry exactly the prod **origin** (scheme+host+port — Tauri offers no path scoping) (→ T1.3) |
| R2 | Ship the derived placeholder identifier (`com.example.*`) | Replace with the real reverse-DNS id **before** `android init`/`ios init` bakes it in (→ T1.4) |
| R3 | Point the shell at `http://`, or at a host with an untrusted cert | Generator rejects non-dev http; mobile webviews render a blank screen on bad TLS, no interstitial (→ T0.2, T6.3b) |
| R4 | Rely on a repo grep to catch `usesCleartextTraffic` | The Android manifest lives only in git-ignored, regenerated `gen/android/` — the durable gate inspects the **built release artifact** (→ T6.3a) |
| R5 | Assume the WebKit-279153 unset-`SameSite` hazard applies to the session cookie | Autumn *always* emits `SameSite` on its session cookie (`build_set_cookie` writes it unconditionally); the hazard applies to cookies Nexus sets **by hand** — test those (→ T0.1) |
| R6 | Session cookie without `Secure` in prod | Framework prod default is `secure = true`; T0.1 pins it against a Nexus override regression |
| R7 | Trust WKWebView cookie persistence | ITP/sync bugs (WebKit bug 213510) randomly drop cookies → long-`Max-Age` server-side session **plus** silent re-auth via a rotating refresh token held in Stronghold/keychain — never a long-lived credential in the plaintext store (→ T4.x, R17) |
| R8 | Ship a bare webview wrapper | App Store Guideline 4.2 rejection bait; wire notification/store/biometric into real Nexus flows (→ T3.x) |
| R9 | White screen in airplane mode | Offline detection + retry view (§5 Phase 5 picks the mechanism) (→ T5.x) |
| R10 | Generate one Tauri mode on top of another | Guard refuses desktop↔thin-client; the in-process `tauri-mobile` mode has **no markers** and is ruled out by review; never hand-delete markers — `autumn destroy tauri` first (→ T1.2) |
| R11 | Embed third-party iframes on pages served to the shell | On Linux/Android an embedded iframe can be treated as the remote origin → it inherits device-API grants. Enforce via the server CSP `frame-src` (→ T0.3), not process memory |
| R12 | Register `/api/` in `security.csrf.exempt_paths` while those routes still accept cookie auth | That is a plain CSRF hole. Preferred: echo `autumn-csrf` into `X-CSRF-Token`. Exemption is permitted **only** for routes that reject cookie auth and require `Authorization` at the extractor level (→ T4.2) |
| R13 | Assume the generated `.gitignore` protects secrets | It covers exactly `/target /binaries /configs /gen` — **signing material is not covered**: `*.jks`, `*.keystore`, `keystore.properties` (cleartext passwords), `*.p12`, `*.mobileprovision`, `AuthKey_*.p8` need explicit rules + a `git ls-files` gate (→ T6.1) |
| R14 | Restrict trusted hosts without the app domain | *If* `security.trusted_hosts.hosts` is non-empty (it defaults empty = unrestricted), it must include the app origin (→ T0.1, conditional) |
| R15 | Prune `core:default` permissions blind | `window.__TAURI__` relies on core event/window plumbing; prune only after on-device verification. The default sets are version-dependent and opaque → snapshot them (→ T1.6) |
| R16 | Serve user-generated HTML/JS/SVG from the granted origin | The grant is origin-wide: a stored XSS or hostile upload on any path of the prod origin becomes native-plugin access + token theft. UGC/attachments are served from a **separate origin** (or `Content-Disposition: attachment`, never inline HTML) (→ T0.4) |
| R17 | Call the plaintext store a secure token vault | `tauri-plugin-store` is plaintext JSON on disk and the biometric gate is enforced by remote JS — bypassed by server compromise or direct file read. Access tokens in it: TTL ≤ 15 min. The persistent credential lives in Stronghold/platform keychain (→ T4.4, §7 decision) |
| R18 | Let the chrome-less window navigate anywhere | An open redirect or phishing link moves the whole window to a foreign origin the user cannot inspect (the grant doesn't follow, but credential phishing does). `on_navigation` allowlist: prod origin in-window, everything else to the system browser (→ T2.4) |
| R19 | Treat the prod domain as "just DNS" | The origin is compiled into shipped binaries as a device-API grant. Registrar lock, auto-renew, expiry monitoring, HSTS; and decide the desktop updater question (§7) |
| R20 | Run the dev-URL scaffold and commit it | The `http://10.0.2.2:3000` variant is a `--force` **overwrite of the same files** (lib.rs + both capability files) — a committed dev scaffold grants device APIs to a cleartext origin. Never commit; use a scratch worktree; T1.3/T6.3b run as non-skippable CI gates precisely for this |

## 3. Planning — Six Thinking Hats

- **White (facts).** Generator file plan is nine files + icons; URL validation
  requires https (http only for `localhost`/`127.0.0.1`/`[::1]`/`10.0.2.2`;
  userinfo rejected; quotes/backslashes/control chars rejected — but **path,
  query, and fragment are accepted** and embedded into `lib.rs`, hence T0.2's
  no-query/no-fragment rule); capability grants carry the URL's *origin*
  (`origin().ascii_serialization()` — no path, no trailing slash, default port
  elided) while `lib.rs` embeds the *full normalized URL* (bare host gains a
  trailing `/`) — two deliberately different strings; `withGlobalTauri: true`
  injects `window.__TAURI__`; `tauri.conf.json` ships `security.csp: null` by
  design — **the server's CSP is the only CSP in force**; mobile projects are
  generated into git-ignored `src-tauri/gen/`; the shell crate is a
  **standalone `[workspace]`** invisible to root-level `cargo` invocations.
  Unverified facts to confirm in the Nexus session: Nexus's production HTTPS
  origin; its autumn version; whether `autumn generate pwa` was already run;
  whether `/api/` routes currently accept cookie auth (gates the R12
  decision); the real issue's AC list.
- **Red (gut).** Thin client feels right: Nexus is online-first and already a
  web service; shipping server fixes without store re-review is a huge win.
  The anxieties: App Store 4.2 review roulette, WKWebView cookie flakiness,
  and — sharpened by review — the origin-wide trust grant. All have concrete
  mitigations; none has a guarantee.
- **Black (caution).** The remote origin is *fully trusted* by the shell: a
  compromised or mis-deployed server can drive every granted device API, and
  because the grant is origin-wide, **any XSS anywhere on the prod origin is a
  device compromise, not a session compromise** — the server CSP (T0.3) and
  UGC origin separation (T0.4) are load-bearing security controls, not
  hygiene. Server outage = app outage on all platforms at once. Desktop has
  no updater → no grant-revocation path (R19). Store review is holistic;
  nothing guarantees approval. No cross-compilation: CI needs a 3-OS matrix
  plus Android/iOS jobs, and the standalone shell workspace needs its own
  `cargo audit`/Dependabot pointing.
- **Yellow (benefits).** One codebase, zero duplicated frontend; sessions,
  Maud/htmx templates, and routes run unmodified; native capabilities via
  three pre-registered official plugins; the same pages progressively enhance
  in plain browsers; instant fleet-wide fixes via server deploys.
- **Green (creative).** Biometric-gated token release as a *local
  unattended-device* control and an App-Store-4.2 signal (not confidentiality
  — R17). Offline draft persistence via the store plugin. Deep links,
  share-sheet, single-instance desktop lock, `tauri-plugin-stronghold` as the
  credential vault. Desktop polish: window size/title, native menu.
- **Blue (process).** Execute in the Nexus repo in TDD phases (§5), each phase
  strictly red → green → refactor with the failing test committed first —
  Phase 0 is explicitly a *regression-pinning* phase (some assertions are
  framework defaults; the phase pins them against Nexus-side overrides, and
  says so). Multi-angle agent review after implementation (security/
  trust-model, Tauri config, auth/session, App-Store-readiness, test
  quality). Then diff §6's proposed ACs against the real issue, map each to
  evidence, implement gaps. Revisit in-process modes only if offline-first
  becomes a requirement.

## 4. Target file plan (what `autumn generate tauri --remote-url` writes)

```
src-tauri/
  tauri.conf.json              productName, identifier (REPLACE placeholder!),
                               withGlobalTauri: true (→ T1.5),
                               security.csp: null (deliberate — server CSP governs, → T0.3),
                               version frozen at generate time (→ AC16: bump per store submission)
  Cargo.toml                   "{app}-mobile" crate; staticlib/cdylib; own [workspace];
                               biometric dep target-gated to android/ios
  build.rs                     tauri_build::build()
  Info.ios.plist               NSFaceIDUsageDescription (Face ID hard-requires it)
  capabilities/
    remote-app.json            core:default, notification:default, store:default
                               @ prod ORIGIN (ascii origin serialization — no path/slash)
    remote-app-mobile.json     biometric:default, platforms: [android, iOS] @ prod ORIGIN
  src/main.rs                  {app}_mobile::run()
  src/lib.rs                   plugin registration + WebviewUrl::External(FULL normalized URL —
                               bare host gains a trailing '/', so this string ≠ the origin string)
  icons/                       placeholders (1×1 rasters; icon.svg reused from PWA if present)
  .gitignore                   /target /binaries /configs /gen   ← does NOT cover signing material
```

`--dry-run` prints the plan without writing; `--force` overwrites within the
mode. `autumn destroy tauri --remote-url <URL>` reverts — it must be given the
**same URL the scaffold was generated with** (revert recomputes the plan from
it), and needs `--force` once identifier/icons/window polish diverge (destroy
refuses on diverged content). Record the exact generate URL in the shipping
runbook (Phase 1 REFACTOR).

## 5. Red / Green / Refactor implementation plan (execute in `wheelhorsedev/nexus`)

Every phase lands as: (RED) committed failing test(s) → (GREEN) minimal change
→ (REFACTOR) clean up, tests green. Scaffold-content tests live in a
`tests/tauri_shell.rs` integration suite; server tests in Nexus's existing
tree; two gates are pipeline stages (marked), not repo tests, because their
subject only exists in the release job.

**Phase 0 — server-side contract (pin inherited prod defaults + real changes).**
Autumn's prod profile already defaults `session.secure = true`, CSRF enabled,
`SameSite=Lax` always emitted, `HttpOnly` on — so several assertions here are
*regression pins*, and are labeled as such; the falsifiable RED targets are
the header-level assertions plus the deltas Nexus actually makes.
- T0.1 RED: drive a request through Nexus's router under the prod profile and
  assert the emitted `Set-Cookie`: contains `; Secure`, `; HttpOnly`,
  `; SameSite=Lax` (a `None` value fails absent a documented cross-context
  exception), and `Max-Age >= 2_592_000` (30 d — a real change from the 86400
  default, chosen for the R7 dropped-cookie story; this is the phase's genuine
  RED). Conditional clause: if `security.trusted_hosts.hosts` is non-empty it
  contains the app origin (test named `…_when_restricted`, no-op when empty).
  Also assert the `autumn-csrf` cookie is `Secure` and **not** `HttpOnly`
  (JS must read it to echo it).
- T0.2 RED: the configured public origin is `https`, has no userinfo, **no
  query, no fragment** (query/fragment survive `validate_remote_url` and get
  baked into committed `lib.rs` — never carry a token in `--remote-url`), and
  **byte-equals** the origin string in both capability files while the
  `lib.rs` literal equals the full normalized URL. This catches "prod domain
  changed, scaffold not regenerated" without re-implementing upstream
  validation.
- T0.3 RED (CSP contract — load-bearing, see §3 Black): prod responses carry
  `Content-Security-Policy` with nonce/hash-based `script-src` (no
  `'unsafe-inline'`/`'unsafe-eval'`), `object-src 'none'`, `base-uri 'self'`,
  `form-action 'self'`, and `frame-src 'none'` or an explicit allowlist
  (mechanizes R11).
- T0.4 RED (UGC origin separation — R16): upload/attachment/avatar routes
  serve from a non-granted origin, or with `Content-Disposition: attachment`
  and never inline HTML/SVG.
- GREEN: raise `session.max_age_secs`, add/adjust CSP, move UGC serving.
- REFACTOR: collect into one "mobile-shell prod contract" test module.

**Phase 1 — Scaffold.**
- T1.1 RED: the files Nexus's contract depends on exist: both capability
  files, `Info.ios.plist`, `src/lib.rs`, `tauri.conf.json`, `.gitignore`
  (superset check, not an exact nine-file count — don't couple to upstream's
  file plan; comment names the autumn version the list came from).
- T1.2 RED: **both directions of mode purity**: no `DESKTOP_MARKERS` file
  exists (`stage-sidecar.sh`/`.ps1`, `tauri.{linux,macos,windows}.conf.json`)
  **and** all three `THIN_CLIENT_MARKERS` exist — the positive half proves the
  tree is still thin-client. (The in-process `tauri-mobile` mode has no
  markers; ruled out by review, not testable.)
- T1.3 RED: parse both capability files; `remote.urls ==
  ["<ascii origin of prod URL>"]` exactly (no wildcard, no second origin, no
  trailing slash); mobile file carries `platforms: ["android","iOS"]` and only
  `biometric:default`. Note: this string differs from the `lib.rs` URL (T0.2).
- T1.4 RED: `tauri.conf.json` `identifier` is the real Nexus reverse-DNS id,
  not `com.example.*`.
- T1.5 RED: `tauri.conf.json` `app.withGlobalTauri === true` — every T3.x
  behavior silently no-ops without it, so this is the injected-global
  contract test (AC14).
- T1.6 RED: snapshot the resolved permission sets of `core:default`,
  `notification:default`, `store:default` (and `biometric:default`) for the
  pinned Tauri version as a committed artifact; CI fails when a version bump
  changes the expansion — a dependency bump must not silently widen the
  device-API grant.
- GREEN: `autumn generate tauri --remote-url https://<nexus-prod>` (after
  `--dry-run` review), replace the placeholder identifier, commit.
- REFACTOR: pin `tauri-cli --version "^2"`; record the exact generate URL in
  the shipping runbook (destroy needs it).

**Phase 2 — Desktop shell (thin client on desktop — this plan's extension).**
- T2.0 RED (local): `src-tauri/Cargo.toml` declares
  `crate-type = ["staticlib", "cdylib", "rlib"]` and gates
  `tauri-plugin-biometric` under
  `[target.'cfg(any(target_os = "android", target_os = "ios"))']` — the
  precondition for desktop compiling at all; fails the moment someone
  un-gates biometric.
- T2.1 (CI-RED — a pipeline state, not a committable repo test; labeled
  honestly): build the shell crate on the ubuntu/macos/windows matrix. The
  crate is a standalone workspace: jobs must `cd src-tauri` (or
  `--manifest-path src-tauri/Cargo.toml`), and `cargo audit`/Dependabot must
  target `src-tauri/Cargo.lock` separately.
- T2.2 RED: desktop polish is a **hand edit** to `lib.rs` (the scaffold sets
  only `.title()`); assert the added `.inner_size(…)`/`.min_inner_size(…)`
  survive regeneration. (This edit is what makes destroy require `--force`.)
- T2.3 RED (Unix desktops): either no credential is persisted on desktop
  (cookie-only — the recommended default), or the store file/dir is hardened
  to `0600`/`0700` in a `#[cfg(unix)]` setup step before first write — the
  thin-client scaffold has none of the sidecar's permission hardening.
- T2.4 RED: `lib.rs` installs an `on_navigation` handler — main-window
  navigation only to the prod origin; every other URL opens in the system
  browser (R18; hand-added, the generated builder has no handler).
- GREEN: make the matrix green, add the hand edits. REFACTOR: reusable CI
  workflow.

**Phase 3 — Native capabilities wired into Nexus pages.**
- T3.1a RED (server-side): the shared shell-detection module
  (`if (window.__TAURI__) …`) is referenced on the target pages, and no
  native-only markup is emitted unconditionally.
- T3.1b RED (headless JS, node/jsdom): import the module with
  `globalThis.__TAURI__` stubbed and with it deleted; the browser branch
  throws nothing and hides native-only UI. (Only this half evidences
  "degrades cleanly" — a Maud render is byte-identical either way, so a
  server-side test cannot observe the degraded branch.)
- T3.2 RED: notification flow (permission request + `sendNotification`) glue
  behind detection.
- T3.3 RED: store-plugin draft/token glue; biometric prompt path gated to
  mobile.
- GREEN: one shared JS module + per-page hooks. REFACTOR: dedupe.

**Phase 4 — Auth/session (design decision stated, then tests).**
Design: cookie sessions remain primary. The *persistent* fallback credential
(R7) is a **rotating refresh token with server-side reuse detection and
per-device binding, stored in Stronghold/platform keychain** — never the
plaintext store. The plaintext store holds at most an access token with
TTL ≤ 15 min. CSRF: **echo the token** (`autumn-csrf` → `X-CSRF-Token`) is the
default; `security.csrf.exempt_paths` may be used *only* for routes that
reject cookie auth entirely.
- T4.1 RED: login issues the short-lived access token; refresh rotates on
  401; logout deletes the store entry, invalidates **all** the account's
  server-side sessions on request, and clears the webview cookie store for
  the origin.
- T4.2 RED: (i) a mutating request to a non-exempt path without
  `X-CSRF-Token`/`_csrf` is rejected; (ii) `security.csrf.exempt_paths`
  contains exactly the intended prefixes (a test that fails if someone adds
  `"/"`); (iii) if any path is exempted, a cookie-only mutating request to it
  is rejected (the extractor requires `Authorization`).
- T4.3 RED: covered by T0.1's `Max-Age` floor + idle-timeout/rotation policy:
  session id rotates on privilege change; absolute lifetime bounded.
- T4.4 RED: replaying a previously-used refresh token invalidates the whole
  device session chain server-side.
- GREEN/REFACTOR as above. (Manual device attestation for the biometric
  prompt itself → AC9b.)

**Phase 5 — Offline UX (mechanism chosen, not hand-waved).**
The scaffold has **no `build` section and no local assets** — "bundle a
fallback page" would reintroduce `frontendDist` (rejected in §1.3). Choose:
- (a) shell-side: an error-path handler in `lib.rs` swapping in an inline
  `data:`/HTML retry document — T5.1a asserts the handler exists and renders
  with zero network; or
- (b) server-side (preferred if PWA is in play): `autumn generate pwa`
  service-worker offline page — T5.1b asserts the SW route + offline page.
- T5.2 RED: page-level `offline` event listener shows a retry view.
- GREEN: implement the chosen one. REFACTOR: shared partial.

**Phase 6 — Mobile init, builds, ship-readiness.**
- T6.1 RED: repo lint — generated four ignore entries intact **plus** signing
  patterns (`*.jks`, `*.keystore`, `keystore.properties`, `*.p12`,
  `*.mobileprovision`, `*.p8`) ignored at root and under `src-tauri/`, and
  `git ls-files` matches none of them. Keys/passwords come from CI secrets.
- T6.2: `cargo tauri android init && … build`, `cargo tauri ios init && …
  build` — automated where CI has the SDKs (AC6a), documented manual gate
  where not (AC6b). Identifier already final (T1.4) because init bakes it in.
- T6.3a (pipeline gate in the release job, after `android init` — produces no
  evidence on a clean checkout and says so): extract the merged manifest from
  the built `.aab`/`.apk` (`bundletool dump manifest` / `aapt2 dump xmltree`);
  fail on `usesCleartextTraffic="true"` or any cleartext-permitting
  `networkSecurityConfig`.
- T6.3b RED (repo test, every commit): the `lib.rs` URL literal and both
  capability `remote.urls` equal the https prod strings (full URL vs origin,
  per T0.2), and no `http://` appears in any tracked `src-tauri/` file —
  this is the gate that catches a committed dev scaffold (R20).
- T6.4 RED: every PNG under `src-tauri/icons/` decodes to ≥ 32×32 (the
  placeholders are 1×1) and `icon.ico`/`icon.icns` exceed a size floor — a
  decoded-property test, not byte-comparison (PWA icon reuse makes `icon.svg`
  differ from the placeholder for the wrong reason; upstream bytes can
  change).
- T6.5 RED: release `Info.ios.plist` contains no `NSAppTransportSecurity` key
  (the iOS dev-loop ATS exception lives in a **tracked** file, unlike the
  Android flag — it must never ship).
- Store submission: signing/provisioning per Tauri's Google Play / App Store
  docs (manual runbook); `tauri.conf.json` `version` bumped per submission
  (AC16 — it is frozen at generate time and does not track the app crate).

**Dev loops** (documented, never committed): the local-server variants
(`--remote-url http://10.0.2.2:3000` for the emulator; `adb reverse tcp:3000
tcp:3000` + `http://localhost:3000` for a device) are `--force` **overwrites
of the same committed files** — run them in a scratch worktree and rely on
T1.3/T6.3b as non-skippable gates. Android: dev-only cleartext via a
`networkSecurityConfig` scoped to the dev host (re-applied after every
`init`), preferred over the blanket attribute. iOS: local-http dev needs an
`NSAppTransportSecurity`/`NSAllowsLocalNetworking` exception in the *tracked*
`Info.ios.plist` — hence T6.5.

## 6. Proposed acceptance criteria (stand-in until the real issue is readable)

The task's issue lives in `wheelhorsedev/nexus` and could not be read from
this session. In the Nexus session: diff this list against the real issue
first, then fill the evidence column (test path / CI run / attestation).
Automated vs manual halves are split explicitly — an AC whose evidence is a
human attestation says so.

| # | Acceptance criterion | Evidence |
|---|---|---|
| 1 | `src-tauri/` matches the thin-client file/content contract for the Nexus prod origin | T1.1, T1.3, T1.4 |
| 2 | Mode purity: no desktop-sidecar files; all thin-client markers present | T1.2 |
| 3 | Capability grants: exactly the prod origin, minimal plugin set, biometric platform-restricted; permission expansions snapshotted | T1.3, T1.6 |
| 4 | Bundle identifier is Nexus's real reverse-DNS id | T1.4 |
| 5a | Desktop shell builds on the 3-OS matrix | T2.1 (CI) |
| 5b | Desktop shell opens the Nexus prod login page on each OS | manual attestation (screenshot per OS) |
| 6a | Android/iOS shells init + build where CI has SDKs | T6.2 |
| 6b | Device install + launch verified | manual attestation |
| 7 | Prod cookie contract: `Secure`, `HttpOnly`, `SameSite=Lax`, `Max-Age ≥ 30 d`; CSRF on; trusted hosts include the origin *when restricted* | T0.1 |
| 8 | Native features used behind `__TAURI__` detection; degraded browser branch proven in a headless JS run | T3.1a + **T3.1b** (only b evidences degradation) |
| 9a | Token issue/refresh/revoke + replay-detection pass server-side | T4.1, T4.2, T4.4 |
| 9b | Biometric prompt releases the token on a real device | manual attestation |
| 10 | Offline shows a friendly retry view, not a white screen | T5.x (chosen mechanism) + manual airplane-mode attestation |
| 11 | No generated/mobile-project/signing files tracked | T6.1 |
| 12 | Release artifacts: no cleartext allowances (built-artifact check), https prod strings only in tracked files, no ATS exception | T6.3a (pipeline), T6.3b, T6.5 |
| 13 | Nexus serves the CSP contract (the shell ships `csp: null` by design) | T0.3 |
| 14 | `withGlobalTauri: true` asserted in CI | T1.5 |
| 15 | No user-controlled HTML/JS served from the granted origin | T0.4 |
| 16 | `tauri.conf.json` `version` bumped per store submission | runbook + release checklist |

Process rules with **no AC by choice** (recorded so the table doesn't look
incomplete): R11 is enforced via AC13's `frame-src`; R15 via AC3's snapshot;
R10's third-mode caveat and R19's domain governance are review/runbook items.

## 7. Decisions required in the Nexus session (not silently deferred)

1. **Desktop updater**: adopt `tauri-plugin-updater`, or accept in writing
   that a shipped desktop grant is irrevocable while the domain is the only
   control (R19).
2. **Stronghold/keychain**: in scope for Phase 4 (recommended — R17/finding
   "refresh token must not live in the plaintext store"), or explicit written
   risk acceptance with the ≤ 15 min access-token TTL as the compensating
   control.
3. **Offline mechanism**: Phase 5 option (a) shell error page vs (b) PWA
   service worker.
4. **Desktop credential policy**: cookie-only (recommended) vs hardened store
   file (T2.3).

Out of scope regardless: deep links, share-sheet, single-instance lock,
in-process/offline-sync modes (`autumn generate tauri-mobile
[--offline-sync]` — implemented upstream, rejected here as architecture).

## 8. Open questions the Nexus session must answer before Phase 0

(a) Nexus's prod origin; (b) its autumn version; (c) whether `autumn generate
pwa` has run; (d) whether `/api/` routes accept cookie auth today (gates the
R12/T4.2 design); (e) the real issue's AC list (replaces §6's stand-ins);
(f) the resolved expansion of `core:default`/`store:default`/
`notification:default` for the pinned Tauri version — including whether
`store:default`'s `load(path)` permits paths outside the app data dir.
