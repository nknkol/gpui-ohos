import { AbilityConstant, abilityAccessCtrl, bundleManager, Permissions, Want } from "@kit.AbilityKit";
import { hilog } from "@kit.PerformanceAnalysisKit";
import window from "@ohos.window";
import { LazyPlugin, NativeAbility } from "@ohos-rs/ability";
import { AppControlPlugin } from "@ohos-rs/ability-plugin-app-control";
import { ClipboardPlugin } from "@ohos-rs/ability-plugin-clipboard";
import { FilesPlugin } from "@ohos-rs/ability-plugin-files";
import { MenuPlugin } from "@ohos-rs/ability-plugin-menu";
import { NotificationPlugin } from "@ohos-rs/ability-plugin-notification";
import { PermissionPlugin } from "@ohos-rs/ability-plugin-permission";
import { SystemStatePlugin } from "@ohos-rs/ability-plugin-system-state";
import { UrlPlugin } from "@ohos-rs/ability-plugin-url";
import { WindowPlugin } from "@ohos-rs/ability-plugin-window";
// __GPUI_EXTRA_IMPORTS__
import * as nativeModule from "libgpui_app.so";

const TAG = "GpuiEntryAbility";
const USER_GRANT_DIR_PERMISSIONS: Permissions[] = [
  "ohos.permission.READ_WRITE_DOCUMENTS_DIRECTORY",
  "ohos.permission.READ_WRITE_DOWNLOAD_DIRECTORY",
  "ohos.permission.READ_WRITE_DESKTOP_DIRECTORY",
];
const FULL_DISK_PERMISSION: Permissions = "ohos.permission.ACCESS_USER_FULL_DISK";

export default class EntryAbility extends NativeAbility {
  public moduleName: string = "gpui_app";
  public defaultPage: boolean = true;
  public bridgePlugins = [
    new LazyPlugin(() => new AppControlPlugin()),
    new LazyPlugin(() => new ClipboardPlugin()),
    new LazyPlugin(() => new FilesPlugin()),
    new LazyPlugin(() => new MenuPlugin()),
    new LazyPlugin(() => new NotificationPlugin({ scheme: "gpui-notification" })),
    new LazyPlugin(() => new PermissionPlugin()),
    new LazyPlugin(() => new SystemStatePlugin()),
    new LazyPlugin(() => new UrlPlugin()),
    new LazyPlugin(() => new WindowPlugin()),
    // __GPUI_EXTRA_PLUGINS__
  ];

  override onCreate(want: Want, launchParam: AbilityConstant.LaunchParam): void {
    if (typeof nativeModule.init !== "function") {
      throw new Error("GPUI native module libgpui_app.so is unavailable");
    }
    // __GPUI_ON_CREATE__
    super.onCreate(want, launchParam);
  }

  override onWindowStageCreate(windowStage: window.WindowStage): void {
    super.onWindowStageCreate(windowStage);
    void this.enqueueLifecycleOperation("gpui-user-file-permissions", async (): Promise<void> => {
      await this.ensureDeclaredUserFilePermissions();
    });
  }

  private async ensureDeclaredUserFilePermissions(): Promise<void> {
    try {
      const flags =
        bundleManager.BundleFlag.GET_BUNDLE_INFO_WITH_REQUESTED_PERMISSION |
        bundleManager.BundleFlag.GET_BUNDLE_INFO_WITH_APPLICATION;
      const bundleInfo = bundleManager.getBundleInfoForSelfSync(flags);
      const tokenId = bundleInfo.appInfo.accessTokenId;
      const declared = new Set<string>(
        (bundleInfo.reqPermissionDetails ?? []).map((item) => item.name),
      );
      const atManager = abilityAccessCtrl.createAtManager();

      const pendingUserGrants: Permissions[] = [];
      for (const perm of USER_GRANT_DIR_PERMISSIONS) {
        if (!declared.has(perm)) {
          continue;
        }
        const status = atManager.checkAccessTokenSync(tokenId, perm);
        hilog.info(0x0000, TAG, "permission check %{public}s => %{public}d", perm, status);
        if (status !== abilityAccessCtrl.GrantStatus.PERMISSION_GRANTED) {
          pendingUserGrants.push(perm);
        }
      }

      if (pendingUserGrants.length > 0) {
        const result = await atManager.requestPermissionsFromUser(this.context, pendingUserGrants);
        hilog.info(
          0x0000,
          TAG,
          "requestPermissionsFromUser(%{public}s) => %{public}s",
          JSON.stringify(pendingUserGrants),
          JSON.stringify(result.authResults),
        );
      }

      if (declared.has(FULL_DISK_PERMISSION)) {
        const fullDiskStatus = atManager.checkAccessTokenSync(tokenId, FULL_DISK_PERMISSION);
        hilog.info(
          0x0000,
          TAG,
          "permission check %{public}s => %{public}d",
          FULL_DISK_PERMISSION,
          fullDiskStatus,
        );
        if (fullDiskStatus !== abilityAccessCtrl.GrantStatus.PERMISSION_GRANTED) {
          try {
            const settingResults = await atManager.openPermissionOnSetting(
              this.context,
              FULL_DISK_PERMISSION,
            );
            hilog.info(
              0x0000,
              TAG,
              "openPermissionOnSetting(%{public}s) => %{public}d",
              FULL_DISK_PERMISSION,
              settingResults,
            );
          } catch (settingErr) {
            hilog.warn(
              0x0000,
              TAG,
              "openPermissionOnSetting(%{public}s) skipped/failed: %{public}s",
              FULL_DISK_PERMISSION,
              String(settingErr),
            );
          }
        }
      }
    } catch (err) {
      hilog.warn(0x0000, TAG, "ensureDeclaredUserFilePermissions failed: %{public}s", String(err));
    }
  }
}
