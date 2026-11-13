// SPDX-License-Identifier: GPL-2.0
/*
 * Replaces fs/namei.c: there are no path lookups, only the name hashes netdev and kernfs code take.
 * These are namei.c's byte-at-a-time versions; a hash only has to agree with itself here.
 */

#include <linux/export.h>
#include <linux/stringhash.h>

unsigned int full_name_hash(const void *salt, const char *name, unsigned int len)
{
	unsigned long hash = init_name_hash(salt);

	while (len--)
		hash = partial_name_hash((unsigned char)*name++, hash);
	return end_name_hash(hash);
}
EXPORT_SYMBOL(full_name_hash);

u64 hashlen_string(const void *salt, const char *name)
{
	unsigned long hash = init_name_hash(salt);
	unsigned long len = 0, c;

	c = (unsigned char)*name;
	while (c) {
		len++;
		hash = partial_name_hash(c, hash);
		c = (unsigned char)name[len];
	}
	return hashlen_create(end_name_hash(hash), len);
}
EXPORT_SYMBOL(hashlen_string);
