// SPDX-License-Identifier: GPL-2.0-or-later
/*
 * Replaces kernel/params.c: there is no command line or sysfs to set module parameters from, so only
 * the lock that readers of a parameter take is needed. Built-in code shares one lock, as in Linux.
 */

#include <linux/module.h>
#include <linux/moduleparam.h>
#include <linux/mutex.h>

static DEFINE_MUTEX(param_lock);

void kernel_param_lock(struct module *mod)
{
	mutex_lock(&param_lock);
}
EXPORT_SYMBOL(kernel_param_lock);

void kernel_param_unlock(struct module *mod)
{
	mutex_unlock(&param_lock);
}
EXPORT_SYMBOL(kernel_param_unlock);
