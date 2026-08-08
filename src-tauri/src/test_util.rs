//! 测试工具 — 串行化会修改进程级环境变量的测试
//! (rust 测试并行运行, 全局 env 竞争会导致偶发失败)

use std::sync::Mutex;

pub static ENV_LOCK: Mutex<()> = Mutex::new(());
