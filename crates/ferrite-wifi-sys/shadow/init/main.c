// SPDX-License-Identifier: GPL-2.0-or-later
/*
 * Replaces init/main.c: the start-up that runs before the initcalls, from the
 * start_kernel and kernel_init parts Genode's lx_emul start.c keeps.
 */

#include <linux/init.h>
#include <linux/maple_tree.h>
#include <linux/radix-tree.h>
#include <linux/wait_bit.h>
#include <linux/workqueue.h>

/* definitions in drivers/base/base.h */
extern int devices_init(void);
extern int buses_init(void);
extern int classes_init(void);
extern int platform_bus_init(void);
extern int faux_bus_init(void);

/* Run on the init thread, once the scheduler and kthreadd are up. */
void lx_kernel_init(void)
{
	wait_bit_init();
	radix_tree_init();
	maple_tree_init();

	workqueue_init_early();
	workqueue_init();

	/* driver_init() of drivers/base/init.c */
	devices_init();
	buses_init();
	classes_init();
	platform_bus_init();
	faux_bus_init();
}
