// CodexFF — 管理员授权助手 (编译为可执行文件 CodexFF 随 App 打包)。
//
// App 在后台线程启动本助手；助手在自己的主线程中通过 NSAppleScript
// 请求管理员授权，避免 App 主线程冻结，也不再使用已废弃的
// AuthorizationExecuteWithPrivileges。
//
// 用法: CodexFF "<shell script>"
// 退出码: 0 = 成功; 2 = 参数错误; 3 = 授权或命令执行失败; 6 = 命令产生输出。

#import <Foundation/Foundation.h>

static NSString *readable_error(NSDictionary *errorInfo) {
    NSNumber *number = errorInfo[NSAppleScriptErrorNumber];
    if (number.integerValue == -128) {
        return @"用户已取消";
    }

    NSString *message = errorInfo[NSAppleScriptErrorMessage];
    if (message.length > 0) {
        return message;
    }
    return @"管理员授权失败，请重试";
}

int main(int argc, const char *argv[]) {
    @autoreleasepool {
        if (argc < 2) {
            fputs("缺少脚本参数\n", stderr);
            return 2;
        }

        NSString *script = [NSString stringWithUTF8String:argv[1]];
        if (script == nil) {
            fputs("脚本参数编码无效\n", stderr);
            return 2;
        }

        // 仅把 Base64 字符串嵌入固定 shell 命令，避免脚本内容进入
        // AppleScript 字符串或产生二次 shell 转义问题。
        NSData *data = [script dataUsingEncoding:NSUTF8StringEncoding];
        NSString *encoded = [data base64EncodedStringWithOptions:0];
        NSString *command = [NSString stringWithFormat:
            @"do shell script \"/bin/echo '%@' | /usr/bin/base64 -D | /bin/sh\" "
             "with administrator privileges",
            encoded
        ];

        NSAppleScript *appleScript =
            [[NSAppleScript alloc] initWithSource:command];
        NSDictionary *errorInfo = nil;
        NSAppleEventDescriptor *result =
            [appleScript executeAndReturnError:&errorInfo];

        if (result == nil) {
            NSString *message = readable_error(errorInfo ?: @{});
            fprintf(stderr, "%s\n", message.UTF8String);
            return 3;
        }

        // 与旧助手保持兼容：networksetup 正常执行时没有输出；
        // 若命令返回文本，则把文本交给 App 作为异常信息显示。
        NSString *output = result.stringValue;
        if (output.length > 0) {
            fprintf(stderr, "%s\n", output.UTF8String);
            return 6;
        }
        return 0;
    }
}
