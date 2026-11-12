/* SPDX-License-Identifier: GPL-2.0 */
/*
 * BUG() and WARN() as calls instead of ud2 plus a __bug_table entry.
 *
 * The x86 header is included for its constants and then BUG() and the warn
 * hook are replaced. The generic warn macros keep working on top of them: a
 * formatted WARN() still prints through __warn_printk() first.
 *
 *   void lx_warn(const char *file, int line)            report, then continue
 *   void lx_bug(const char *file, int line) noreturn    report and stop
 *
 * Both are the shim's to implement; lx_bug must not return.
 */
#ifndef _LX_SHADOW_ASM_BUG_H
#define _LX_SHADOW_ASM_BUG_H

#include_next <asm/bug.h>

#ifndef __ASSEMBLER__

extern void lx_warn(const char *file, int line);
extern void lx_bug(const char *file, int line) __noreturn;

#undef BUG
#define BUG() lx_bug(__FILE__, __LINE__)

/* The once-only flag is per call site, as the kernel's __bug_table flag was. */
#undef __WARN_FLAGS
#define __WARN_FLAGS(flags)						\
do {									\
	static bool __lx_warned;					\
	if (!((flags) & BUGFLAG_ONCE) || !__lx_warned) {		\
		__lx_warned = true;					\
		lx_warn(__FILE__, __LINE__);				\
	}								\
} while (0)

#endif /* !__ASSEMBLER__ */

#endif
