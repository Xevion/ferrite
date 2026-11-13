// SPDX-License-Identifier: GPL-2.0-or-later
/*
 * Replaces kernel/printk/printk.c. A message is formatted into one buffer and
 * handed to the shim's logger (ferrite-lkpi `printk`) with its kernel level.
 * One core and no preemption inside a call, so the buffer is not shared.
 * Adapted from Genode's lx_emul shadow (Stefan Kalkowski, Genode Labs).
 */

#include <linux/kernel.h>
#include <linux/printk.h>
#include <linux/stdarg.h>

extern void lx_printk(int level, const char *msg, size_t len);

static char print_buffer[1024];

static int emit(int level, const char *fmt, va_list args)
{
	int len = vscnprintf(print_buffer, sizeof(print_buffer), fmt, args);
	const char *msg = print_buffer;
	int prefix;

	/* A level prefix in the text wins over the caller's default. */
	while ((prefix = printk_get_level(msg)) != 0) {
		if (prefix >= '0' && prefix <= '7')
			level = prefix - '0';
		msg = printk_skip_level(msg);
		len -= 2;
	}
	lx_printk(level, msg, len);
	return len;
}

asmlinkage int vprintk_emit(int facility, int level, const struct dev_printk_info *dev_info,
			    const char *fmt, va_list args)
{
	return emit(level < 0 ? MESSAGE_LOGLEVEL_DEFAULT : level, fmt, args);
}
EXPORT_SYMBOL(vprintk_emit);

asmlinkage int vprintk(const char *fmt, va_list args)
{
	return emit(MESSAGE_LOGLEVEL_DEFAULT, fmt, args);
}
EXPORT_SYMBOL(vprintk);

asmlinkage __visible int _printk(const char *fmt, ...)
{
	va_list args;
	int ret;

	va_start(args, fmt);
	ret = vprintk(fmt, args);
	va_end(args);
	return ret;
}
EXPORT_SYMBOL(_printk);

/* Deferral avoids console locks that do not exist here, so it is the same call. */
asmlinkage __visible int _printk_deferred(const char *fmt, ...)
{
	va_list args;
	int ret;

	va_start(args, fmt);
	ret = vprintk(fmt, args);
	va_end(args);
	return ret;
}
