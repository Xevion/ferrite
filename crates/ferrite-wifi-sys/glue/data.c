// SPDX-License-Identifier: GPL-2.0
/* Kernel data with a deliberate value, kept out of the generated no-access storage. */

#include <linux/delay.h>
#include <linux/export.h>

/* Zero as in Genode: udelay goes to the shim, and workqueue's CPU-intensive threshold then clamps to its default. */
unsigned long loops_per_jiffy;
EXPORT_SYMBOL(loops_per_jiffy);

#include <linux/kobject.h>

/* NULL as in Genode: kernel/ksysfs.c is not built, and a NULL parent puts a kobject at the root of a sysfs nobody sees. */
struct kobject *kernel_kobj;
EXPORT_SYMBOL(kernel_kobj);
