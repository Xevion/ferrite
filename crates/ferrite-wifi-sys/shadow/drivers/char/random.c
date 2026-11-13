// SPDX-License-Identifier: GPL-2.0
/*
 * Replaces drivers/char/random.c. Random bytes come from the shim's generator
 * (ferrite-lkpi `random`), which the app seeds from the platform's entropy, so
 * the pool is always initialized. Adapted from Genode's lx_emul shadow
 * (Stefan Kalkowski, Genode Labs).
 */

#include <linux/notifier.h>
#include <linux/random.h>

extern void lx_random_bytes(void *buf, size_t len);

void get_random_bytes(void *buf, size_t len)
{
	lx_random_bytes(buf, len);
}
EXPORT_SYMBOL(get_random_bytes);

u16 get_random_u16(void)
{
	u16 value;

	lx_random_bytes(&value, sizeof(value));
	return value;
}
EXPORT_SYMBOL(get_random_u16);

u32 get_random_u32(void)
{
	u32 value;

	lx_random_bytes(&value, sizeof(value));
	return value;
}
EXPORT_SYMBOL(get_random_u32);

u64 get_random_u64(void)
{
	u64 value;

	lx_random_bytes(&value, sizeof(value));
	return value;
}
EXPORT_SYMBOL(get_random_u64);

/* Lemire's multiply-and-reject draw from [0, ceil), as drivers/char/random.c does. */
u32 __get_random_u32_below(u32 ceil)
{
	u32 rand = get_random_u32();
	u64 mult = (u64)ceil * rand;

	if (unlikely((u32)mult < ceil)) {
		u32 bound = -ceil % ceil;

		while (unlikely((u32)mult < bound))
			mult = (u64)ceil * get_random_u32();
	}
	return mult >> 32;
}
EXPORT_SYMBOL(__get_random_u32_below);

int wait_for_random_bytes(void)
{
	return 0;
}
EXPORT_SYMBOL(wait_for_random_bytes);

int execute_with_initialized_rng(struct notifier_block *nb)
{
	nb->notifier_call(nb, 0, NULL);
	return 0;
}
EXPORT_SYMBOL(execute_with_initialized_rng);
