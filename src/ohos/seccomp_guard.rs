//! HarmonyOS application sandbox seccomp `SIGSYS -> -ENOSYS` guard.
//!
//! HarmonyOS PC / OpenHarmony application sandboxes intercept modern Linux syscalls
//! (`unshare` 97, `rseq` 293, `clone3` 435, `close_range` 436, `openat2` 437,
//! `epoll_pwait2` 441, `landlock_*` 444..446, `futex_waitv` 449, `fchmodat2` 452)
//! using `SECCOMP_RET_TRAP`, delivering `SIGSYS` (signal 31) with `PC` already
//! advanced past `svc #0` rather than returning `-ENOSYS`.
//!
//! Upstream Rust crates (`rustix`, `cap-primitives`, `polling`, `async-io`, `notify`,
//! `std::process::Command`) probe these syscalls expecting `ENOSYS` so they can cache
//! unavailability and fall back to portable POSIX syscalls (`openat`, `clone`,
//! `/proc/self/fd` + `fcntl(FD_CLOEXEC)`, `epoll_pwait`, `fchmodat`).
//!
//! Installing a resident, zero-stack AArch64 `SIGSYS` trap handler converts kernel
//! `SYS_SECCOMP` traps directly into `x0 = -38` (`-ENOSYS`), letting both inline
//! `svc #0` (`rustix`) and musl `syscall()` (`__syscall_ret(-38)` -> `errno = ENOSYS`)
//! take their standard fallbacks transparently without per-crate source patches.

#[cfg(all(target_arch = "aarch64", target_env = "ohos"))]
mod imp {
    use core::arch::{asm, global_asm};
    use std::sync::atomic::{AtomicBool, Ordering};

    const NR_RT_SIGACTION: usize = 134;
    const NR_RT_SIGPROCMASK: usize = 135;
    const SIG_UNBLOCK: usize = 1;
    const SIGSYS_NUM: usize = 31;
    const SIGSYS_MASK_BIT: u64 = 1u64 << (SIGSYS_NUM - 1);
    /// `SA_SIGINFO (4) | SA_RESTORER (0x0400_0000) | SA_ONSTACK (0x0800_0000) | SA_RESTART (0x1000_0000) | SA_NODEFER (0x4000_0000)`
    const SA_FLAGS_SECCOMP_TRAP: usize = 0x5c00_0004;

    static INSTALLED: AtomicBool = AtomicBool::new(false);

    #[repr(C)]
    struct KernelSigAction {
        sa_handler: usize,
        sa_flags: usize,
        sa_restorer: usize,
        sa_mask: u64,
    }

    global_asm!(
        ".text",
        ".balign 4",
        ".global __gpui_ohos_sigsys_restorer",
        ".hidden __gpui_ohos_sigsys_restorer",
        ".type __gpui_ohos_sigsys_restorer, %function",
        "__gpui_ohos_sigsys_restorer:",
        "    mov x8, #139", // __NR_rt_sigreturn
        "    svc #0",
        ".size __gpui_ohos_sigsys_restorer, . - __gpui_ohos_sigsys_restorer",
        ".balign 4",
        ".global __gpui_ohos_sigsys_handler",
        ".hidden __gpui_ohos_sigsys_handler",
        ".type __gpui_ohos_sigsys_handler, %function",
        "__gpui_ohos_sigsys_handler:",
        "    cbz x1, 1f",
        "    cbz x2, 1f",
        "    ldr w4, [x1, #8]",   // siginfo_t->si_code
        "    cmp w4, #1",         // SYS_SECCOMP == 1
        "    b.ne 1f",
        "    mov x3, #-38",       // -ENOSYS
        "    str x3, [x2, #184]", // ucontext->uc_mcontext.regs[0] = -ENOSYS
        "1:",
        "    ret",
        ".size __gpui_ohos_sigsys_handler, . - __gpui_ohos_sigsys_handler",
    );

    unsafe extern "C" {
        fn __gpui_ohos_sigsys_handler();
        fn __gpui_ohos_sigsys_restorer();
    }

    #[inline]
    unsafe fn raw_syscall4(nr: usize, a0: usize, a1: usize, a2: usize, a3: usize) -> isize {
        let ret: isize;
        unsafe {
            asm!(
                "svc #0",
                in("x8") nr,
                inout("x0") a0 => ret,
                in("x1") a1,
                in("x2") a2,
                in("x3") a3,
                options(nostack),
            );
        }
        ret
    }

    /// Ensure `SIGSYS` is unblocked on the current thread so kernel `force_sig_seccomp`
    /// never escalates a blocked `SECCOMP_RET_TRAP` to `SIG_DFL` termination.
    pub fn unblock_sigsys_current_thread() {
        let mask: u64 = SIGSYS_MASK_BIT;
        unsafe {
            let _ = raw_syscall4(
                NR_RT_SIGPROCMASK,
                SIG_UNBLOCK,
                (&raw const mask) as usize,
                0,
                core::mem::size_of::<u64>(),
            );
        }
    }

    /// Install the process-wide `SIGSYS -> -ENOSYS` seccomp trap handler and unblock
    /// `SIGSYS` on the calling thread. Safe and idempotent to call multiple times
    /// (including inside `Command::pre_exec` or native child processes).
    pub fn install_seccomp_sigsys_guard() -> bool {
        unblock_sigsys_current_thread();

        let ksa = KernelSigAction {
            sa_handler: __gpui_ohos_sigsys_handler as *const () as usize,
            sa_flags: SA_FLAGS_SECCOMP_TRAP,
            sa_restorer: __gpui_ohos_sigsys_restorer as *const () as usize,
            sa_mask: 0,
        };
        let rc = unsafe {
            raw_syscall4(
                NR_RT_SIGACTION,
                SIGSYS_NUM,
                (&raw const ksa) as usize,
                0,
                core::mem::size_of::<u64>(),
            )
        };
        let ok = rc == 0;
        if ok {
            INSTALLED.store(true, Ordering::Release);
        }
        ok
    }

    pub fn is_seccomp_sigsys_guard_installed() -> bool {
        INSTALLED.load(Ordering::Acquire)
    }

    extern "C" fn init_seccomp_guard_ctor() {
        let _ = install_seccomp_sigsys_guard();
    }

    #[used]
    #[link_section = ".init_array"]
    static SECCOMP_GUARD_CTOR: extern "C" fn() = init_seccomp_guard_ctor;
}

#[cfg(not(all(target_arch = "aarch64", target_env = "ohos")))]
mod imp {
    pub fn unblock_sigsys_current_thread() {}

    pub fn install_seccomp_sigsys_guard() -> bool {
        false
    }

    pub fn is_seccomp_sigsys_guard_installed() -> bool {
        false
    }
}

pub use imp::{
    install_seccomp_sigsys_guard, is_seccomp_sigsys_guard_installed, unblock_sigsys_current_thread,
};
