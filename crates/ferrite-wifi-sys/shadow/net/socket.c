// SPDX-License-Identifier: GPL-2.0-or-later
/*
 * Replaces net/socket.c: kernel sockets only. There are no system calls and no sockfs, so a socket is
 * the socket_alloc sockfs would make, from kmalloc, with a zeroed inode (owner root) that nothing reads
 * beyond its owner and mode. Protocol families register as in Linux.
 */

#include <linux/net.h>
#include <linux/printk.h>
#include <linux/slab.h>
#include <linux/spinlock.h>
#include <linux/stat.h>
#include <linux/wait.h>
#include <net/busy_poll.h>
#include <net/sock.h>

#ifdef CONFIG_NET_RX_BUSY_POLL
unsigned int sysctl_net_busy_read __read_mostly;
unsigned int sysctl_net_busy_poll __read_mostly;
#endif

static DEFINE_SPINLOCK(net_family_lock);
static const struct net_proto_family __rcu *net_families[NPROTO] __read_mostly;

struct socket *sock_alloc(void)
{
	struct socket_alloc *ei = kzalloc(sizeof(*ei), GFP_KERNEL);
	struct socket *sock;

	if (!ei)
		return NULL;
	sock = &ei->socket;
	init_waitqueue_head(&sock->wq.wait);
	sock->state = SS_UNCONNECTED;
	ei->vfs_inode.i_mode = S_IFSOCK | S_IRWXUGO;
	return sock;
}
EXPORT_SYMBOL(sock_alloc);

int sock_create_lite(int family, int type, int protocol, struct socket **res)
{
	struct socket *sock = sock_alloc();

	*res = sock;
	if (!sock)
		return -ENOMEM;
	sock->type = type;
	return 0;
}
EXPORT_SYMBOL(sock_create_lite);

void sock_release(struct socket *sock)
{
	const struct proto_ops *ops = READ_ONCE(sock->ops);

	if (ops) {
		ops->release(sock);
		sock->sk = NULL;
		sock->ops = NULL;
		module_put(ops->owner);
	}
	kfree(container_of(sock, struct socket_alloc, socket));
}
EXPORT_SYMBOL(sock_release);

int sock_register(const struct net_proto_family *ops)
{
	int err = 0;

	if (ops->family >= NPROTO) {
		pr_crit("protocol %d >= NPROTO(%d)\n", ops->family, NPROTO);
		return -ENOBUFS;
	}
	spin_lock(&net_family_lock);
	if (rcu_dereference_protected(net_families[ops->family], lockdep_is_held(&net_family_lock)))
		err = -EEXIST;
	else
		rcu_assign_pointer(net_families[ops->family], ops);
	spin_unlock(&net_family_lock);
	pr_info("NET: Registered protocol family %d\n", ops->family);
	return err;
}
EXPORT_SYMBOL(sock_register);

void sock_unregister(int family)
{
	BUG_ON(family < 0 || family >= NPROTO);
	spin_lock(&net_family_lock);
	RCU_INIT_POINTER(net_families[family], NULL);
	spin_unlock(&net_family_lock);
	synchronize_rcu();
}
EXPORT_SYMBOL(sock_unregister);
