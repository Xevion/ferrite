/* SPDX-License-Identifier: GPL-2.0 */
/*
 * Softirqs run on the stack of the task that runs them: tasks have no irq
 * stack to switch to.
 */
#ifndef _ASM_X86_SOFTIRQ_STACK_H
#define _ASM_X86_SOFTIRQ_STACK_H

#include <linux/interrupt.h>

static inline void do_softirq_own_stack(void)
{
	__do_softirq();
}

#endif
