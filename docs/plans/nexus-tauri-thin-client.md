# Nexus in a Tauri Shell (Desktop + Mobile) — Thin-Client Planning Dossier

Status: **planning deliverable only** — produced in a session bound to
`autumn-foundation/egregore`, which cannot attach the private Nexus repository
(`wheelhorsedev/nexus`; cross-tier attach refused, and its issue tracker is
likewise unreachable from here). Implementation, the red/green/refactor test
work, the multi-angle code review, and the issue-AC evidence table must be
executed in a session started **from `wheelhorsedev/nexus`**. This dossier is
written to be carried into that session and executed as-is.

Ground truth used (read in full, not summarized from memory):

- `autumn-foundation/autumn` `docs/guide/tauri-mobile-thin-client.md`
  (the guide the task names) and `docs/guide/tauri.md` (desktop sidecar guide).
- `autumn-cli/src/generate/tauri.rs` — `plan_tauri_thin_client`,
  `validate_remote_url`, `ensure_no_opposite_mode_scaffold`,
  `THIN_CLIENT_MARKERS` / `DESKTOP_MARKERS`, and the rendered file templates.

## 0. The one architecture decision this plan encodes

**Both desktop and mobile use the same thin-client scaffold**:
`autumn generate tauri --remote-url https://<nexus-prod-domain>` — one
`src-tauri/` sub-project whose webview loads the cloud-hosted Nexus server over
HTTPS. There is no sidecar, no bundled database, and no staging step on any
target.

Why this is safe for desktop even though the guide titles the mode "mobile":
the generated shell crate compiles on desktop by design (the guide and the
generator comments both rely on desktop `cargo tauri dev` smoke-test builds);
the notification and store plugins compile on every target; only the biometric
plugin is target-gated to Android/iOS via a `platforms`-restricted capability
file. Desktop thin client is therefore the same scaffold plus (optional)
desktop window polish — not a second mode. Critically, the generator **refuses
to mix modes in one tree** (`ensure_no_opposite_mode_scaffold`, enforced even
under `--force`), so "sidecar on desktop + thin client on mobile" would demand
two scaffold trees and is deliberately NOT this plan. If a self-contained
desktop app is ever required, that is a mode switch
(`autumn destroy tauri --remote-url … && autumn generate tauri`), tracked as a
separate piece of work.

## 1. Planning — Brainstorming (divergent options)

1. **A single thin-client scaffold for all targets** (chosen; rest of this
   dossier). One deployment serves browsers, PWA, desktop shell, and mobile
   shells; a server fix updates every installed app instantly.
2. **Desktop sidecar + mobile thin client.** Full offline desktop, but two
   architectures to maintain, two auth stories, and the generator's mode guard
   makes them mutually exclusive in one tree. Rejected for now.
3. **`frontendDist` as a remote URL** (config-only form, no Rust window code).
   Rejected: the generator deliberately avoids it — `tauri dev` has a known bug
   with URL-form `frontendDist` (tauri-apps/tauri#12333) — and the Rust-side
   `WebviewUrl::External` builder keeps the URL next to the plugin
   registration.
4. **PWA only, no Tauri.** Zero store presence, no native plugins (biometric,
   native notifications, native key-value store), no Guideline-4.2 story.
   Rejected as the sole path; note `autumn generate pwa` composes with Tauri
   and its icon is auto-reused by the Tauri scaffold.
5. **In-process backend options** (autumn roadmap issue #1506 Option A = thin
   client; #1507 Option B = in-process + remote DB; #1508 Option C = local
   SQLite + sync). B/C are explicit follow-ups if offline-first ever becomes a
   requirement; they change nothing about the scaffold landed here.
6. **Value-add ideas to fold in** (cheap now, high leverage): biometric-gated
   token release; offline retry/fallback page bundled in the shell; a shared
   `window.__TAURI__` detection JS module so every Nexus page degrades
   gracefully in plain browsers; store-plugin draft persistence for flaky
   mobile networks.

## 2. Planning — Reverse brainstorming ("how would we guarantee failure?")

Each failure recipe below becomes a mitigation and, where testable, a TDD test
in §5.

| # | Guaranteed-failure move | Mitigation (→ test) |
|---|---|---|
| R1 | Widen `remote.urls` to `https://*` or add extra origins | Capability files carry exactly one origin, the prod origin (→ T1.3) |
| R2 | Ship with the derived placeholder identifier (`com.example.*`) | Replace with real reverse-DNS id **before** `android init`/`ios init` bakes it in (→ T1.4) |
| R3 | Point the shell at `http://`, or at a host with an untrusted cert | Generator rejects non-dev http; mobile webviews show a blank screen on bad TLS, no interstitial (→ T0.2, T6.3) |
| R4 | Leave `android:usesCleartextTraffic="true"` in a release build | Dev-only flag; release check greps `gen/android` config (→ T6.3) |
| R5 | Rely on unset-`SameSite` cookie defaults | iOS 18.0 briefly flipped WKWebView's unset default (WebKit bug 279153); set `session.same_site` explicitly (→ T0.1) |
| R6 | Session cookie without `Secure` in prod | `SameSite=None` is rejected without it; cookie could transit plaintext (→ T0.1) |
| R7 | Trust WKWebView cookie persistence | ITP/sync bugs (WebKit bug 213510) randomly drop cookies → long-`Max-Age` `HttpOnly` server-side session + silent token-refresh fallback (→ T4.x) |
| R8 | Ship a bare webview wrapper | App Store Guideline 4.2 rejection bait; wire notification/store/biometric into real Nexus flows (→ T3.x) |
| R9 | White screen in airplane mode | Offline detection + retry view, small bundled fallback page (→ T5.x) |
| R10 | Generate desktop sidecar on top of the thin client (or vice versa) | Mode guard refuses; never bypass by hand-deleting markers — `autumn destroy tauri` first (process rule, and → T1.2 asserts no `DESKTOP_MARKERS` file exists) |
| R11 | Embed third-party iframes on pages served to the shell | On Linux/Android Tauri can treat an embedded iframe as the remote origin → it inherits device-API grants (process rule + template review gate) |
| R12 | Bearer-token `fetch` posts silently fail CSRF | Echo `autumn-csrf` into `X-CSRF-Token`, or register `/api/` under `security.csrf.exempt_paths` (→ T4.2) |
| R13 | Commit `src-tauri/gen/`, binaries, or signing secrets | Generated `.gitignore` covers `/target /binaries /configs /gen`; CI lint asserts (→ T6.1) |
| R14 | Forget trusted-hosts allow-listing | If `AUTUMN_SECURITY__TRUSTED_HOSTS__HOSTS` is restricted, include the app domain (→ T0.1) |
| R15 | Prune `core:default` permissions blind | `window.__TAURI__` relies on core event/window plumbing; prune only after on-device verification (process rule) |

## 3. Planning — Six Thinking Hats

- **White (facts).** The generator exists and is documented; its file plan is
  nine files + icons; URL validation requires https (http only for
  `localhost`/`127.0.0.1`/`::1`/`10.0.2.2`, userinfo rejected); capability
  grants are scoped to the URL's *origin*; `withGlobalTauri: true` injects
  `window.__TAURI__` into remote pages; mobile projects are generated into
  git-ignored `src-tauri/gen/` by `cargo tauri android|ios init`. Unverified
  facts to confirm in the Nexus session: Nexus's production HTTPS origin, its
  autumn version, whether `autumn generate pwa` was already run, and the real
  issue's AC list.
- **Red (gut).** Thin client feels right: Nexus is online-first and already a
  web service; shipping server fixes without store re-review is a huge win.
  The two anxieties are App Store 4.2 review roulette and WKWebView cookie
  flakiness — both have concrete mitigations (§2 R7/R8), neither has a
  guarantee.
- **Black (caution).** A compromised (or merely mis-deployed) server can drive
  every granted device API — the remote origin is fully trusted, so the
  capability grant is a security boundary and must stay one-origin, minimal
  permission. Server outage = app outage on all platforms at once. No
  auto-updater is scaffolded for the desktop shell (server code updates
  instantly, but shell-binary fixes need reinstalls) — acceptable for v1,
  listed as a follow-up. Store review is holistic; nothing guarantees
  approval. Cross-compilation isn't a thing: CI needs a 3-OS matrix plus
  Android/iOS jobs.
- **Yellow (benefits).** One codebase, zero duplicated frontend; sessions,
  Maud/htmx templates, and routes run unmodified; native capabilities arrive
  through three pre-registered official plugins; the same pages progressively
  enhance in plain browsers; instant fleet-wide fixes via server deploys.
- **Green (creative).** Biometric-gated token release (`authenticate` →
  read token from store plugin) turns a review-risk checkbox into a real
  security feature. Offline draft persistence via the store plugin. Deep
  links, share-sheet integration, and `tauri-plugin-stronghold` (encrypted
  at-rest secrets) as follow-ups. Desktop polish: window size/title, native
  menu, single-instance lock.
- **Blue (process).** Execute in the Nexus repo in TDD phases (§5), each phase
  strictly red → green → refactor with the failing test committed first.
  Multi-angle agent review after implementation (security/trust-model, Tauri
  config correctness, auth/session, App-Store-readiness, test-quality). Then
  map the real issue's ACs to evidence; implement any gap. Revisit options
  B/C only if an offline-first requirement lands.

## 4. Target file plan (what `autumn generate tauri --remote-url` writes)

```
src-tauri/
  tauri.conf.json              productName, identifier (REPLACE placeholder!), withGlobalTauri
  Cargo.toml                   "{app}-mobile" crate; staticlib/cdylib for android/ios init
  build.rs                     tauri_build::build()
  Info.ios.plist               NSFaceIDUsageDescription (Face ID hard-requires it)
  capabilities/
    remote-app.json            core:default, notification:default, store:default @ prod origin
    remote-app-mobile.json     biometric:default, platforms: [android, iOS] @ prod origin
  src/main.rs                  {app}_mobile::run()
  src/lib.rs                   plugin registration + WebviewUrl::External(prod URL)
  icons/                       placeholders — replace, then `cargo tauri icon`
  .gitignore                   /target /binaries /configs /gen
```

`--dry-run` prints the plan without writing; `--force` overwrites within the
mode; `autumn destroy tauri --remote-url <URL>` reverts.

## 5. Red / Green / Refactor implementation plan (execute in `wheelhorsedev/nexus`)

Every phase lands as: (RED) committed failing test(s) → (GREEN) minimal change
to pass → (REFACTOR) clean up with tests staying green. Tests that assert on
generated files live in a `tests/tauri_shell.rs` (or equivalent) integration
suite in the Nexus repo; server-config and auth tests live in Nexus's existing
test tree.

**Phase 0 — Server-side prerequisites (before any scaffold).**
- T0.1 RED: config tests assert prod profile has `session.secure = true`, an
  *explicit* `session.same_site`, CSRF middleware enabled, and — if trusted
  hosts are restricted — the app origin allow-listed.
- T0.2 RED: a test that the configured remote URL parses, is `https`, and has
  no userinfo (mirror of `validate_remote_url`, so a config regression fails
  in Nexus CI, not at scaffold time).
- GREEN: set the knobs (`AUTUMN_SESSION__SECURE`, `session.same_site`, …).
- REFACTOR: collect these into one "mobile-shell prod contract" test module.

**Phase 1 — Scaffold.**
- T1.1 RED: test asserts all nine thin-client files exist under `src-tauri/`.
- T1.2 RED: test asserts **no** `DESKTOP_MARKERS` file exists (no
  `stage-sidecar.*`, no `tauri.*.conf.json` overlays) — guards R10 forever.
- T1.3 RED: parse both capability files; assert `remote.urls == [prod origin]`
  exactly (no wildcard, no second origin), assert the mobile file carries
  `platforms: ["android","iOS"]` and only `biometric:default`.
- T1.4 RED: parse `tauri.conf.json`; assert `identifier` is the real Nexus
  reverse-DNS id and **not** `com.example.*`.
- GREEN: run `autumn generate tauri --remote-url https://<nexus-prod>`
  (after `--dry-run` review), replace the placeholder identifier, commit.
- REFACTOR: pin `tauri-cli --version "^2"` in CI/docs.

**Phase 2 — Desktop shell (thin client on desktop).**
- T2.1 RED: CI job `cargo build` (or `cargo tauri build --debug`) of the shell
  crate on ubuntu/macos/windows matrix — fails until toolchain deps
  (webkit2gtk-4.1 etc.) and any workspace wiring are in place.
- T2.2 RED (if window polish wanted): assert configured window title/size in
  `tauri.conf.json` / `lib.rs`.
- GREEN: make the matrix green; REFACTOR: extract a reusable CI workflow.

**Phase 3 — Native capabilities wired into Nexus pages.**
- T3.1 RED: template/render tests assert the shared shell-detection JS module
  (`if (window.__TAURI__) …`) ships on the relevant pages and that pages render
  correctly when the global is absent (plain browser).
- T3.2 RED: notification flow — permission request + `sendNotification` glue
  present behind detection.
- T3.3 RED: store-plugin draft/token glue present; biometric prompt path gated
  to mobile.
- GREEN: implement one shared JS module + per-page hooks. REFACTOR: dedupe.

**Phase 4 — Auth/session hardening (R7/R12).**
- T4.1 RED: token handoff endpoints — short-lived access token issued at
  login, refresh endpoint rotates on 401, logout deletes store entry AND
  invalidates server-side.
- T4.2 RED: CSRF interplay — token-authenticated `/api/` paths either exempt
  via `security.csrf.exempt_paths` or requests echo `X-CSRF-Token`; a mutating
  bearer request without either must fail in test.
- T4.3 RED: session cookie is `HttpOnly`, `Secure`, long `Max-Age`
  (persistent, not browser-session).
- GREEN/REFACTOR as above.

**Phase 5 — Offline UX (R9).**
- T5.1 RED: shell bundles a local fallback/retry page (or page-load error
  handler) — test asserts the asset/handler exists and is referenced.
- T5.2 RED: page-level `offline` event listener shows a retry view.
- GREEN: implement minimal friendly retry; REFACTOR into shared partial.

**Phase 6 — Mobile init, builds, ship-readiness.**
- T6.1 RED: repo lint — `src-tauri/gen/`, `binaries/`, signing material never
  tracked; `.gitignore` intact.
- T6.2: `cargo tauri android init && cargo tauri android build`,
  `cargo tauri ios init && cargo tauri ios build` in CI (or documented manual
  gate where CI lacks macOS/Android SDK) — identifier already final from T1.4.
- T6.3 RED: release checks — no `usesCleartextTraffic` in release Android
  config; remote URL in `lib.rs` is the https prod URL.
- T6.4: replace placeholder icons (`cargo tauri icon`); test asserts icon
  bytes differ from the generator's placeholders.
- Signing/provisioning/store submission per Tauri's Google Play / App Store
  distribution docs (manual, documented in the repo's shipping runbook).

**Dev loops** (documented, not tested): Android emulator against a local
server via `--remote-url http://10.0.2.2:3000` scaffold variant or
`adb reverse tcp:3000 tcp:3000` + `http://localhost:3000`; dev-only cleartext
flag scoped and removed before release.

## 6. Proposed acceptance criteria (stand-in until the real issue is readable)

The task's issue lives in `wheelhorsedev/nexus` and could not be read from
this session, so the AC⇄evidence table cannot be filled honestly here. The
following proposed ACs are derived from the task statement + the thin-client
guide; in the Nexus session, first diff them against the real issue, then map
each to evidence (test file, CI run, artifact):

1. `src-tauri/` thin-client scaffold exists, generated by
   `autumn generate tauri --remote-url` against the Nexus prod origin (T1.1).
2. No desktop-sidecar mode files exist in the tree (T1.2).
3. Capability grants name exactly one origin — the prod origin — with the
   minimal plugin set; biometric grant is platform-restricted (T1.3).
4. Bundle identifier is Nexus's real reverse-DNS id (T1.4).
5. Desktop shell builds and opens the remote app on Linux, macOS, and Windows
   (T2.1 CI matrix + a manual screenshot per OS).
6. Android and iOS shells init and build; artifacts produced (T6.2).
7. Prod session config: `Secure`, explicit `SameSite`, CSRF on, trusted hosts
   include the app origin (T0.1).
8. Native features are genuinely used by Nexus pages behind `__TAURI__`
   detection, degrading cleanly in browsers (T3.x) — the Guideline-4.2 story.
9. Token handoff with biometric-gated release works end-to-end; logout revokes
   server-side (T4.x).
10. Offline shows a friendly retry view, not a white screen (T5.x).
11. No generated/mobile-project/secret files tracked in git (T6.1).
12. Release artifacts contain no cleartext-HTTP allowances and point only at
    the https prod URL (T6.3).

## 7. Follow-ups explicitly out of scope

- Desktop auto-update (`tauri-plugin-updater`) for the shell binary itself.
- `tauri-plugin-stronghold` for encrypted-at-rest tokens.
- Offline-first architectures (autumn roadmap Options B/C — issues #1507/#1508).
- Deep links / share-sheet / single-instance desktop polish.
