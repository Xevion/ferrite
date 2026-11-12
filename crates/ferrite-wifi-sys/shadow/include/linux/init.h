/* SPDX-License-Identifier: GPL-2.0 */
/*
 * Initcalls and exitcalls as named global pointers instead of linker-section
 * entries, so the shim can find them without a linker script.
 *
 *   int (*__lx_initcall_<level>_<fn>)(void)    <level> is the initcall level:
 *                                              early, 0, 1, 1s, ... 7, 7s, rootfs
 *   void (*__lx_exitcall_<fn>)(void)
 *
 * The functions stay static. build.rs lists the pointers in the ordered tables
 * lx_initcalls and lx_exitcalls, and fails when two files define the same
 * pointer (two static functions of one name at one level).
 * A console_initcall fails the build.
 */
#ifndef _LX_SHADOW_LINUX_INIT_H
#define _LX_SHADOW_LINUX_INIT_H

#include_next <linux/init.h>

#undef ___define_initcall
#undef __define_initcall
#undef __exitcall

#define __lx_define_initcall(fn, id)					\
	initcall_t __PASTE(__lx_initcall_, __PASTE(id, __PASTE(_, fn)))	\
		__used = fn;

#define ___define_initcall(fn, id, __sec) __lx_define_initcall(fn, id)
#define __define_initcall(fn, id) __lx_define_initcall(fn, id)

#define __exitcall(fn) exitcall_t __PASTE(__lx_exitcall_, fn) __used = fn;

#endif
