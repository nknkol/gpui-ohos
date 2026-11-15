# `gpui-ohos-pack` — Universal HarmonyOS PC HAP Packager for GPUI

`gpui-ohos-pack` turns any Rust `gpui` / `gpui-kit` `cdylib` crate into a signed, ready-to-run HarmonyOS PC (API 26 Stage-model) `.hap` application—with **< 0.5s incremental native repacking**, **zero hardcoded paths**, and first-class support for **HNP (`.hnp`) packages**, **multi-`.so` child sandboxes**, and **custom ArkTS bridges**.

## Quick Start

Configure your app either via `ohos-app.json` in your project root OR directly inside `Cargo.toml` under `[package.metadata.ohos]` (or `[workspace.metadata.ohos]`):

```toml
[package.metadata.ohos]
name = "ndsh"
bundle_name = "ai.ndsh.desktop"
vendor = "ndsh"
app_name = "NDSH"
icon = "assets/icon.svg"
version_code = 100
version_name = "0.1.0"
package = "ndsh-app"
so_name = "libndsh.so"
device_types = ["2in1", "tablet"]
acls = [
  "ohos.permission.CUSTOM_SANDBOX",
  "ohos.permission.READ_WRITE_USER_FILE",
  "ohos.permission.ACCESS_USER_FULL_DISK",
  "ohos.permission.READ_PASTEBOARD",
]
optional_acls = [
  "ohos.permission.MANAGE_SKILL",
]
```

Then build, sign, install, and launch on HarmonyOS PC:

```bash
python3 /path/to/gpui-ohos/packager/gpui-ohos-pack .
```

You can also inspect auto-discovered toolchains or invoke `cargo` directly with the full `aarch64-unknown-linux-ohos` NDK environment pre-configured:

```bash
python3 /path/to/gpui-ohos/packager/gpui-ohos-pack --doctor
python3 /path/to/gpui-ohos/packager/gpui-ohos-pack --cargo check --all-targets
```

## Supported Fields (`ohos-app.json` camelCase or `Cargo.toml` snake_case)

| Field | Description |
|---|---|
| `name` | Output artifact base name (`<name>-signed.hap`) |
| `bundleName` / `bundle_name` | HarmonyOS bundle identifier (e.g. `ai.ndsh.desktop`) |
| `appName` / `app_name` | User-visible launcher title |
| `icon`, `iconBackground`, `iconScale` | `.svg` / `.png` / `.ico` source icon, auto-rendered to `1024×1024` with HarmonyOS rounded mask |
| `manifestPath` / `package` / `soName` / `features` | Primary GPUI `cdylib` Cargo target |
| `extraCargoTargets` | Additional workspace `cdylib` targets packed into `libs/arm64-v8a/` (e.g. `libndsh_sandbox.so` for `OH_Ability_StartNativeChildProcessWithConfigs`) |
| `extraLibs` | Additional prebuilt `.so` files packed into `libs/arm64-v8a/` |
| `hnpPackages` | Bundled `.hnp` native archives (`{ "path": "...", "type": "private", "buildCmd": [...] }`), auto-declared in `module.json5` and stored (`ZIP_STORED`) under `hnp/arm64-v8a/` |
| `rawfiles` / `resfiles` | Static files or directories copied into `resources/rawfile/` or `resources/resfile/` |
| `extraEts`, `abilityImports`, `abilityPlugins`, `abilityOnCreate` | Inject custom ArkTS bridge modules into `EntryAbility.ets` without forking the host skeleton |
| `permissions`, `acls`, `optionalAcls` | `requestPermissions` entries, required `--acl` grants, and optional ACLs that automatically fall back if rejected by the active debug certificate |

## Environment Overrides

All toolchains are auto-discovered from Harmonybrew (`~/.harmonybrew`) and `/data/service/hnp/*`, and can be overridden via:
- `OHOS_SDK_ROOT` / `OHOS_NDK_HOME`
- `HMOS_CLT_ROOT` / `DEVECO_SDK_HOME`
- `HDA_BIN`, `HDC_BIN`, `HDC_TARGET`, `HNPCLI_BIN`, `BINARY_SIGN_TOOL`
- `GPUI_OHOS_CACHE_DIR` (defaults to `${XDG_CACHE_HOME:-~/.cache}/gpui-ohos-packager`)
