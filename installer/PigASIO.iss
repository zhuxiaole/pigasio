; PigASIO 安装包(Inno Setup)
;
; 怎么出包:
;   1. cargo build --release        先出 release 产物(DLL + 控制面板 + CLI)
;   2. iscc installer\PigASIO.iss   编译本脚本
;   成品落在 dist\ 下。
;
; 版本号不用管:build.rs 会从工作区的 Cargo.toml 里取出来写成 dist\version.iss,
; 下面直接包含。改了 Cargo.toml 的 version,重新 cargo build 一次即可。

#define AppName "PigASIO"
#define Publisher "PigASIO contributors"
#define GuiExe "pigasio-gui.exe"
#define CliExe "pigasio.exe"
#define DriverDll "pigasio_asio.dll"

; 版本号来自 Cargo.toml。没先跑过 cargo build --release 的话退回到占位值,
; 好让脚本本身还能编译通过(产物虽然不能用,但报错信息清楚)。
#ifexist "..\target\version.iss"
  #include "..\target\version.iss"
#else
  #define AppVer "0.0.0-not-built"
#endif

[Setup]
; AppId 是升级/卸载的识别依据,一旦发布就不要再改。
AppId={{7E2F4B91-3C5A-4E8D-9B6F-1A2C3D4E5F60}
AppName={#AppName}
AppVersion={#AppVer}
AppPublisher={#Publisher}
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
OutputDir=..\dist
OutputBaseFilename={#AppName}-{#AppVer}-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
; ASIO 驱动只做了 64 位:32 位下 ASIO 用 __thiscall,Rust 稳定版表达不了。
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; 写 HKLM 注册表(驱动注册)必须提权,干脆整个安装程序都以管理员跑。
PrivilegesRequired=admin
SetupIconFile=..\assets\pigasio.ico
UninstallDisplayIcon={app}\{#GuiExe}

[Languages]
Name: "cn"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"

[Tasks]
Name: "desktopicon"; Description: "创建桌面快捷方式"; GroupDescription: "附加任务:"; Flags: unchecked

[Files]
Source: "..\target\release\{#DriverDll}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\{#GuiExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\{#CliExe}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName} 控制面板"; Filename: "{app}\{#GuiExe}"
Name: "{group}\卸载 {#AppName}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#AppName} 控制面板"; Filename: "{app}\{#GuiExe}"; Tasks: desktopicon

[Run]
; 注册 ASIO 驱动。安装程序本身就以管理员身份运行,所以这一步有权限写 HKLM。
; 注册动作复用 CLI —— 注册表和代码里的 CLSID 只有一个出处,不会走偏。
Filename: "{app}\{#CliExe}"; Parameters: "install ""{app}\{#DriverDll}"""; StatusMsg: "正在注册 ASIO 驱动…"; Flags: runhidden waituntilterminated

; 装完顺手把控制面板打开
Filename: "{app}\{#GuiExe}"; Description: "启动 {#AppName} 控制面板"; Flags: postinstall nowait skipifsilent

[UninstallRun]
; 反注册必须赶在文件被删掉之前 —— UninstallRun 正好是这个时机。
; skipifdoesntexist:用户要是自己删过 CLI,卸载也不该卡住。
; RunOnceId:升级重装时卸载逻辑可能被触发多次,靠它保证只反注册一次。
Filename: "{app}\{#CliExe}"; Parameters: "uninstall"; Flags: runhidden waituntilterminated skipifdoesntexist; RunOnceId: "UnregisterAsioDriver"

[Code]
// 注册没成功要让用户当场知道。否则他装完了在 DAW 的驱动列表里找不到 PigASIO,
// 只会以为是软件坏了 —— 而真正的原因往往是没有管理员权限。
procedure CurStepChanged(CurStep: TSetupStep);
var
  CliPath: String;
begin
  if CurStep = ssPostInstall then
  begin
    if not RegKeyExists(HKEY_LOCAL_MACHINE, 'SOFTWARE\ASIO\{#AppName}') then
    begin
      CliPath := ExpandConstant('{app}\{#CliExe}');
      MsgBox('ASIO 驱动没有注册成功。' + #13#10 + #13#10 +
             '这时在控制面板或 DAW 的驱动列表里都看不到 PigASIO。' + #13#10 +
             '可以手动重试:以管理员身份运行' + #13#10 +
             '"' + CliPath + '" install',
             mbError, MB_OK);
    end;
  end;
end;
