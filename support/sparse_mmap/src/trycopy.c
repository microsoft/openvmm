// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#include <assert.h>
#include <stdbool.h>
#include <stdint.h>
#include <string.h>

#if defined(_WIN32)
#include <windows.h>
#else
#include <stdlib.h>
#include <fcntl.h>
#include <setjmp.h>
#include <signal.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>
#include <errno.h>
#endif

struct access_failure {
    void *address;
#if !defined(_WIN32)
    int signal;
    int si_code;
#endif
};

#if defined(_WIN32)

/// Exception filter for try_memmove.
static int exception_filter(EXCEPTION_POINTERS *exc, struct access_failure *failure)
{
    if (exc->ExceptionRecord->ExceptionCode == EXCEPTION_ACCESS_VIOLATION)
    {
        *failure = (struct access_failure) { .address = (void *)exc->ExceptionRecord->ExceptionInformation[1] };
        return EXCEPTION_EXECUTE_HANDLER;
    }
    else
    {
        return EXCEPTION_CONTINUE_SEARCH;
    }
}

/// Simple wrapper around memmove.
///
/// This is necessary for LLVM to compile the SEH __try/__except block in
/// try_memmove correctly. Presumably this is because of this limitation in
/// clang 12, at least, combined with memmove being a special function.
///
/// https://clang.llvm.org/docs/MSVCCompatibility.html
///
/// Asynchronous Exceptions (SEH): Partial.
///
///     Structured exceptions (__try / __except / __finally) mostly work on x86
///     and x64. LLVM does not model asynchronous exceptions, so it is currently
///     impossible to catch an asynchronous exception generated in the same
///     frame as the catching __try.
static void memmove_wrapper(void* dest, void* src, uintptr_t length)
{
    memmove(dest, src, length);
}

// On Windows, just use Structured Exception Handling (SEH) to
// attempt the memmove.
int try_memmove(void* dest, void* src, uintptr_t length, struct access_failure* failure)
{
    __try
    {
        memmove_wrapper(dest, src, length);
        return 0;
    }
    __except(exception_filter(GetExceptionInformation(), failure))
    {
        return -1;
    }
}

static void memset_wrapper(void* dest, int c, uintptr_t length)
{
    memset(dest, c, length);
}

int try_memset(void* dest, int c, uintptr_t length, struct access_failure* failure)
{
    __try
    {
        memset_wrapper(dest, c, length);
        return 0;
    }
    __except(exception_filter(GetExceptionInformation(), failure))
    {
        return -1;
    }
}

// Make sure to include the wrapper function for SEH handling to work with LLVM.
#define TRY_READ(name, type) \
    static type name ## _wrapper(const volatile type* src) \
    { \
        return *src; \
    } \
    int name(const volatile type* src, type* dest, struct access_failure* failure) \
    { \
        __try { \
            *dest = name ## _wrapper(src); \
            return 0; \
        } __except(exception_filter(GetExceptionInformation(), failure)) { \
            return -1; \
        } \
    }

#define TRY_WRITE(name, type) \
    static void name ## _wrapper(volatile type* dest, type value) \
    { \
        *dest = value; \
    } \
    int name(volatile type* dest, type value, struct access_failure* failure) \
    { \
        __try { \
            name ## _wrapper(dest, value); \
            return 0; \
        } __except(exception_filter(GetExceptionInformation(), failure)) { \
            return -1; \
        } \
    }

#define TRY_CMPXCHG(name, intrinsic, type) \
    bool name ## _wrapper(type *dest, type *expected, type desired) \
    { \
        type old = intrinsic((void*)dest, desired, *expected); \
        if (old == *expected) { \
            return true; \
        } else { \
            *expected = old; \
            return false; \
        } \
    } \
    int name(type *dest, type *expected, type desired, struct access_failure *failure) \
    { \
        __try { \
            return name ## _wrapper(dest, expected, desired); \
        } __except(exception_filter(GetExceptionInformation(), failure)) { \
                return -1; \
        } \
    }

#define TRY_WORD(size, intrinsic) \
    TRY_READ(try_read ## size, int ## size ## _t) \
    TRY_WRITE(try_write ## size, int ## size ## _t) \
    TRY_CMPXCHG(try_cmpxchg ## size, intrinsic, int ## size ## _t)

TRY_WORD(8, _InterlockedCompareExchange8)
TRY_WORD(16, _InterlockedCompareExchange16)
TRY_WORD(32, _InterlockedCompareExchange)
TRY_WORD(64, _InterlockedCompareExchange64)

#else

// Keep track of this thread's jump point to return failure
// if memmove touches an invalid page.
__thread struct access_failure * volatile signal_access_failure;
__thread sigjmp_buf signal_jmp_buf;

static void handle_signal(int sig, siginfo_t *info, __attribute__((unused)) void *ucontext)
{
    // Only handle the signal if we're in the middle of a memmove, with the
    // jump point set on this thread.
    if (signal_access_failure)
    {
        *signal_access_failure = (struct access_failure) { .address = info->si_addr, .signal = sig, .si_code = info->si_code };
        signal_access_failure = NULL;

        // siglongjmp out of the signal handler.
        siglongjmp(signal_jmp_buf, 1);
    }
    else
    {
        // Restore the default handler and continue to crash the process.
        struct sigaction act = { .sa_handler = SIG_DFL };
        sigemptyset(&act.sa_mask);
        sigaction(sig, &act, NULL);
    }
}

int install_signal_handlers()
{
    // Install signal handler for SIGSEGV.
    //
    // SA_NODEFER is required due to siglongjmp.
    struct sigaction act = { .sa_sigaction = &handle_signal, .sa_flags = SA_NODEFER | SA_SIGINFO };

    // Don't block any signals.
    if (sigemptyset(&act.sa_mask) == -1)
    {
        return errno;
    }

    static const int signals[] = { SIGSEGV, SIGBUS };

    for (size_t i = 0; i < sizeof(signals) / sizeof(signals[0]); i++)
    {
        int sig = signals[i];

        // Connect the signal handler.
        if (sigaction(sig, &act, NULL) == -1)
        {
            return errno;
        }
    }

    return 0;
}

// On UNIX, hook SIGSEGV across the memmove to determine if the
// copy succeeded or failed.
#define TRY_OP(failure, op) \
    if (signal_access_failure) \
    { \
        abort(); \
    } \
    \
    signal_access_failure = (failure); \
    \
    if (sigsetjmp(signal_jmp_buf, 0) == 0) { \
        op; \
        signal_access_failure = NULL; \
    } else { \
        return -1; \
    }

int try_memmove(void *dest, void *src, uintptr_t length, struct access_failure *failure)
{
    TRY_OP(failure, memmove(dest, src, length));
    return 0;
}

int try_memset(void *dest, int c, uintptr_t length, struct access_failure *failure)
{
    TRY_OP(failure, memset(dest, c, length));
    return 0;
}

#define TRY_READ(name, type) \
    int name(type *dest, const volatile type *src, struct access_failure *failure) \
    { \
        TRY_OP(failure, *dest = *src); \
        return 0; \
    }

#define TRY_WRITE(name, type) \
    int name(volatile type *dest, type value, struct access_failure *failure) \
    { \
        TRY_OP(failure, *dest = value); \
        return 0; \
    }

#define TRY_CMPXCHG(name, type) \
    int name(type *dest, type *expected, type desired, struct access_failure *failure) \
    { \
        bool success; \
        TRY_OP(failure, success = __atomic_compare_exchange_n(dest, expected, desired, false, __ATOMIC_SEQ_CST, __ATOMIC_SEQ_CST)); \
        return success; \
    } \

#define TRY_WORD(size) \
    TRY_READ(try_read ## size, int ## size ## _t) \
    TRY_WRITE(try_write ## size, int ## size ## _t) \
    TRY_CMPXCHG(try_cmpxchg ## size, int ## size ## _t)

TRY_WORD(8)
TRY_WORD(16)
TRY_WORD(32)
TRY_WORD(64)

#endif
