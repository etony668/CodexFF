// CodexFF — 管理员授权助手 (编译为可执行文件 CodexFF 随 App 打包)。
//
// 为什么需要它: macOS 26 禁止直接复制/改名系统 osascript 运行 (SIGKILL),
// 且 Authorization Services 不能在 tokio 后台线程调用 (XPC API misuse)。
// 这个小助手由 App 以子进程方式启动, 在自己的主线程里完成授权,
// 系统授权框显示的调用方名称 = 本进程名 (CodexFF)。
//
// 用法: CodexFF "<shell script>"
// 退出码: 0 = 成功; 2 = 参数错误; 3/4/5 = 授权失败; 6 = 命令输出错误信息。

#include <Security/Authorization.h>
#include <stdio.h>
#include <string.h>

static const char *status_message(OSStatus s) {
    if (s == errAuthorizationSuccess) return "";
    if (s == errAuthorizationCanceled) return "用户已取消";
    if (s == errAuthorizationDenied) return "授权被拒绝";
    if (s == errAuthorizationInteractionNotAllowed) return "无法弹出授权窗口，请重试";
    if (s == errAuthorizationToolExecuteFailure) return "特权命令执行失败，请重试";
    return "管理员授权失败，请重试";
}

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "缺少脚本参数\n");
        return 2;
    }

    AuthorizationRef auth = NULL;
    OSStatus s = AuthorizationCreate(NULL, NULL, kAuthorizationFlagDefaults, &auth);
    if (s != errAuthorizationSuccess) {
        fprintf(stderr, "管理员授权失败，请重试\n");
        return 3;
    }

    AuthorizationItem item = { "system.privilege.admin", 0, NULL, 0 };
    AuthorizationRights rights = { 1, &item };
    AuthorizationFlags flags = kAuthorizationFlagInteractionAllowed
        | kAuthorizationFlagExtendRights
        | kAuthorizationFlagPreAuthorize;
    s = AuthorizationCopyRights(auth, &rights, NULL, flags, NULL);
    if (s != errAuthorizationSuccess) {
        const char *msg = status_message(s);
        if (*msg == '\0') msg = "管理员授权失败，请重试";
        fprintf(stderr, "%s\n", msg);
        AuthorizationFree(auth, kAuthorizationFlagDefaults);
        return 4;
    }

    char *args[] = { "-c", argv[1], NULL };
    FILE *pipe = NULL;
    s = AuthorizationExecuteWithPrivileges(auth, "/bin/sh", kAuthorizationFlagDefaults, args, &pipe);
    AuthorizationFree(auth, kAuthorizationFlagDefaults);
    if (s != errAuthorizationSuccess) {
        const char *msg = status_message(s);
        if (*msg == '\0') msg = "特权命令执行失败，请重试";
        fprintf(stderr, "%s\n", msg);
        return 5;
    }

    // 读取命令输出 (stdout+stderr 已合并): networksetup 失败时会输出 ** Error,
    // 有输出即视为失败, 回传给 App 显示。
    if (pipe) {
        char buf[4096];
        int has_output = 0;
        while (fgets(buf, sizeof(buf), pipe)) {
            fputs(buf, stderr);
            has_output = 1;
        }
        if (has_output) {
            return 6;
        }
    }
    return 0;
}
