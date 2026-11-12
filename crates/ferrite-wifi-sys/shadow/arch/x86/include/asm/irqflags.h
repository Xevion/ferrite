/* SPDX-License-Identifier: GPL-2.0 */
/*
 * Interrupt masking for a uniprocessor with no interrupt handlers of its own.
 *
 * The kernel masks interrupts with cli/sti, which are privileged and would fault
 * when this C runs as a Linux process. Every primitive calls out to the shim:
 *
 *   unsigned long lx_irq_save(void)       mask, return the state to give back
 *   void lx_irq_restore(unsigned long)    return to a state lx_irq_save made
 *   void lx_irq_disable(void)             mask
 *   void lx_irq_enable(void)              unmask
 *   unsigned long lx_irq_flags(void)      the current state, unchanged
 *
 * A state word has bit 0 set when masked. The other bits belong to the shim
 * (the UEFI one keeps the TPL it must return to). The lab shim does nothing and
 * reports 0.
 */
#ifndef _LX_SHADOW_ASM_IRQFLAGS_H
#define _LX_SHADOW_ASM_IRQFLAGS_H

#ifndef __ASSEMBLER__

#include <linux/types.h>

extern unsigned long lx_irq_save(void);
extern void lx_irq_restore(unsigned long flags);
extern void lx_irq_disable(void);
extern void lx_irq_enable(void);
extern unsigned long lx_irq_flags(void);

static __always_inline unsigned long arch_local_save_flags(void)
{
	return lx_irq_flags();
}

static __always_inline void arch_local_irq_disable(void)
{
	lx_irq_disable();
}

static __always_inline void arch_local_irq_enable(void)
{
	lx_irq_enable();
}

static __always_inline unsigned long arch_local_irq_save(void)
{
	return lx_irq_save();
}

static __always_inline void arch_local_irq_restore(unsigned long flags)
{
	lx_irq_restore(flags);
}

static __always_inline int arch_irqs_disabled_flags(unsigned long flags)
{
	return flags & 1;
}

static __always_inline int arch_irqs_disabled(void)
{
	return arch_irqs_disabled_flags(arch_local_save_flags());
}

#endif /* !__ASSEMBLER__ */

#endif
