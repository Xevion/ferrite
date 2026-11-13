// SPDX-License-Identifier: GPL-2.0-or-later
/*
 * Replaces net/socket.c: kernel sockets only. There are no system calls and no sockfs, so a socket is
 * the socket_alloc sockfs would make, from kmalloc, with a zeroed inode (owner root) that nothing reads
 * beyond its owner and mode. Protocol families register as in Linux, and the kernel socket calls
 * (create, bind, send, receive) are stock. At the end, lx_nl_* give the programs a netlink client
 * socket, as a userspace process has through the system calls.
 */

#include <linux/jiffies.h>
#include <linux/net.h>
#include <linux/netlink.h>
#include <linux/uio.h>
#include <linux/printk.h>
#include <linux/slab.h>
#include <linux/spinlock.h>
#include <linux/stat.h>
#include <linux/wait.h>
#include <net/busy_poll.h>
#include <net/net_namespace.h>
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

int __sock_create(struct net *net, int family, int type, int protocol, struct socket **res, int kern)
{
	const struct net_proto_family *pf;
	struct socket *sock;
	int err;

	if (family < 0 || family >= NPROTO)
		return -EAFNOSUPPORT;
	if (type < 0 || type >= SOCK_MAX)
		return -EINVAL;

	sock = sock_alloc();
	if (!sock)
		return -ENFILE;
	sock->type = type;

	rcu_read_lock();
	pf = rcu_dereference(net_families[family]);
	err = -EAFNOSUPPORT;
	if (!pf || !try_module_get(pf->owner)) {
		rcu_read_unlock();
		goto out_release;
	}
	rcu_read_unlock();

	err = pf->create(net, sock, protocol, kern);
	if (err < 0)
		goto out_module_put;
	if (!try_module_get(sock->ops->owner)) {
		err = -EAFNOSUPPORT;
		goto out_module_put;
	}
	module_put(pf->owner);
	*res = sock;
	return 0;

out_module_put:
	sock->ops = NULL;
	module_put(pf->owner);
out_release:
	sock_release(sock);
	return err;
}
EXPORT_SYMBOL(__sock_create);

int sock_create_kern(struct net *net, int family, int type, int protocol, struct socket **res)
{
	return __sock_create(net, family, type, protocol, res, 1);
}
EXPORT_SYMBOL(sock_create_kern);

int sock_sendmsg(struct socket *sock, struct msghdr *msg)
{
	struct sockaddr_storage *save_addr = (struct sockaddr_storage *)msg->msg_name;
	struct sockaddr_storage address;
	int save_len = msg->msg_namelen;
	int ret;

	if (msg->msg_name) {
		memcpy(&address, msg->msg_name, msg->msg_namelen);
		msg->msg_name = &address;
	}
	ret = READ_ONCE(sock->ops)->sendmsg(sock, msg, msg_data_left(msg));
	msg->msg_name = save_addr;
	msg->msg_namelen = save_len;
	return ret;
}
EXPORT_SYMBOL(sock_sendmsg);

int kernel_sendmsg(struct socket *sock, struct msghdr *msg, struct kvec *vec, size_t num, size_t size)
{
	iov_iter_kvec(&msg->msg_iter, ITER_SOURCE, vec, num, size);
	return sock_sendmsg(sock, msg);
}
EXPORT_SYMBOL(kernel_sendmsg);

int sock_recvmsg(struct socket *sock, struct msghdr *msg, int flags)
{
	return READ_ONCE(sock->ops)->recvmsg(sock, msg, msg_data_left(msg), flags);
}
EXPORT_SYMBOL(sock_recvmsg);

int kernel_recvmsg(struct socket *sock, struct msghdr *msg, struct kvec *vec, size_t num, size_t size,
		   int flags)
{
	msg->msg_control_is_user = false;
	iov_iter_kvec(&msg->msg_iter, ITER_DEST, vec, num, size);
	return sock_recvmsg(sock, msg, flags);
}
EXPORT_SYMBOL(kernel_recvmsg);

int kernel_bind(struct socket *sock, struct sockaddr *addr, int addrlen)
{
	struct sockaddr_storage address;

	memcpy(&address, addr, addrlen);
	return READ_ONCE(sock->ops)->bind(sock, (struct sockaddr *)&address, addrlen);
}
EXPORT_SYMBOL(kernel_bind);

/*
 * A netlink socket of protocol in init_net, set up as libnl's nl_connect does: socket(AF_NETLINK,
 * SOCK_RAW, protocol), extended acks on, and bound to a port of its own. An unbound socket has port 0,
 * which broadcasts from the kernel skip as the sender's own.
 */
int lx_nl_open(int protocol, struct socket **res)
{
	struct sockaddr_nl local = { .nl_family = AF_NETLINK };
	int one = 1;
	int err = sock_create_kern(&init_net, PF_NETLINK, SOCK_RAW, protocol, res);

	if (err)
		return err;
	err = READ_ONCE((*res)->ops)->setsockopt(*res, SOL_NETLINK, NETLINK_EXT_ACK, KERNEL_SOCKPTR(&one),
						 sizeof(one));
	if (!err)
		err = kernel_bind(*res, (struct sockaddr *)&local, sizeof(local));
	if (err) {
		sock_release(*res);
		*res = NULL;
	}
	return err;
}

/* setsockopt(NETLINK_ADD_MEMBERSHIP): join multicast group, numbered from 1. */
int lx_nl_join(struct socket *sock, u32 group)
{
	return READ_ONCE(sock->ops)->setsockopt(sock, SOL_NETLINK, NETLINK_ADD_MEMBERSHIP,
						KERNEL_SOCKPTR(&group), sizeof(group));
}

/* The socket's port id, which the kernel addresses replies to. */
u32 lx_nl_portid(struct socket *sock)
{
	struct sockaddr_nl addr = {};

	if (READ_ONCE(sock->ops)->getname(sock, (struct sockaddr *)&addr, 0) < 0)
		return 0;
	return addr.nl_pid;
}

/* sendto the kernel (port 0): the bytes sent, or a negative errno. */
int lx_nl_send(struct socket *sock, const void *buf, size_t len)
{
	struct sockaddr_nl to = { .nl_family = AF_NETLINK };
	struct kvec vec = { .iov_base = (void *)buf, .iov_len = len };
	struct msghdr msg = { .msg_name = &to, .msg_namelen = sizeof(to) };

	return kernel_sendmsg(sock, &msg, &vec, 1, len);
}

/*
 * recv one datagram into buf, sleeping on the socket's wait queue for at most timeout_ms: its length
 * (the whole length when it was truncated, with *flags carrying MSG_TRUNC), -EAGAIN when the time ran
 * out, or another negative errno.
 */
int lx_nl_recv(struct socket *sock, void *buf, size_t len, unsigned int timeout_ms, int *flags)
{
	struct kvec vec = { .iov_base = buf, .iov_len = len };
	struct msghdr msg = {};
	int ret;

	WRITE_ONCE(sock->sk->sk_rcvtimeo, timeout_ms ? msecs_to_jiffies(timeout_ms) : 0);
	ret = kernel_recvmsg(sock, &msg, &vec, 1, len, MSG_TRUNC | (timeout_ms ? 0 : MSG_DONTWAIT));
	*flags = msg.msg_flags;
	return ret;
}

void lx_nl_close(struct socket *sock)
{
	sock_release(sock);
}
