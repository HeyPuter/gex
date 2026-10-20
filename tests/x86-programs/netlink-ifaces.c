/* The interface list through a NETLINK_ROUTE socket, as libc reads it:
 * if_nameindex (musl's and glibc's both dump RTM_GETLINK;
 * gex refused the socket, so if_nameindex failed with ENOBUFS and Wine's nsi
 * knew no interface), the SIOCGIF* ioctls, getifaddrs (RTM_GETADDR too), and getaddrinfo with
 * AI_ADDRCONFIG, which glibc answers from the same dump and which must still
 * give an IPv4 address (apt's http method).
 *
 * Prints only what any machine with a loopback and one IPv4 network has, so
 * the output is the same natively and under gex. */
#include <arpa/inet.h>
#include <errno.h>
#include <ifaddrs.h>
#include <net/if.h>
#include <netdb.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <unistd.h>

int main(void)
{
    int fail = 0;
    struct if_nameindex *ni = if_nameindex();
    int lo = 0, n = 0;
    if (!ni) {
        perror("if_nameindex");
        fail = 1;
    } else {
        for (struct if_nameindex *p = ni; p->if_index; p++, n++)
            if (!strcmp(p->if_name, "lo")) lo = p->if_index;
        if_freenameindex(ni);
    }
    printf("if_nameindex: lo %s, %s one interface\n", lo ? "listed" : "missing", n > 1 ? "more than" : "only");
    /* The interface ioctls on an internet datagram socket, as Wine's nsi asks
     * them (musl's if_nametoindex asks on an AF_UNIX datagram socket, which
     * gex does not have). */
    struct ifreq ifr = { 0 };
    int fd = socket(AF_INET, SOCK_DGRAM | SOCK_CLOEXEC, 0);
    strcpy(ifr.ifr_name, "lo");
    int idx = fd >= 0 && !ioctl(fd, SIOCGIFINDEX, &ifr) ? ifr.ifr_ifindex : -1;
    printf("SIOCGIFINDEX(lo) %s if_nameindex\n", idx == lo ? "matches" : "differs from");
    int up = !ioctl(fd, SIOCGIFFLAGS, &ifr) && (ifr.ifr_flags & (IFF_UP | IFF_LOOPBACK)) == (IFF_UP | IFF_LOOPBACK);
    printf("SIOCGIFFLAGS(lo): %s\n", up ? "up, loopback" : "not up and loopback");
    memset(&ifr, 0, sizeof ifr);
    ifr.ifr_ifindex = lo;
    int named = !ioctl(fd, SIOCGIFNAME, &ifr) && !strcmp(ifr.ifr_name, "lo");
    printf("SIOCGIFNAME(%s): %s\n", "lo's index", named ? "lo" : "wrong");
    strcpy(ifr.ifr_name, "lo");
    int mtu = !ioctl(fd, SIOCGIFMTU, &ifr) && ifr.ifr_mtu > 0;
    printf("SIOCGIFMTU(lo): %s\n", mtu ? "positive" : "missing");
    strcpy(ifr.ifr_name, "no-such-if0");
    int nodev = ioctl(fd, SIOCGIFINDEX, &ifr) == -1 && errno == ENODEV;
    printf("SIOCGIFINDEX(no-such-if0): %s\n", nodev ? "ENODEV" : "not ENODEV");
    if (fd >= 0) close(fd);
    if (!lo || idx != lo || n < 2 || !up || !named || !mtu || !nodev) fail = 1;

    struct ifaddrs *ifa = NULL;
    int lo4 = 0, other4 = 0;
    if (getifaddrs(&ifa) != 0) {
        perror("getifaddrs");
        fail = 1;
    } else {
        for (struct ifaddrs *p = ifa; p; p = p->ifa_next) {
            if (!p->ifa_addr || p->ifa_addr->sa_family != AF_INET) continue;
            struct in_addr a = ((struct sockaddr_in *)p->ifa_addr)->sin_addr;
            if (ntohl(a.s_addr) == INADDR_LOOPBACK && !strcmp(p->ifa_name, "lo")) lo4 = 1;
            else if (ntohl(a.s_addr) >> 24 != 127) other4 = 1;
        }
        freeifaddrs(ifa);
    }
    printf("getifaddrs: 127.0.0.1 on lo %s, another IPv4 address %s\n", lo4 ? "yes" : "no", other4 ? "yes" : "no");
    if (!lo4 || !other4) fail = 1;

    struct addrinfo hints = { 0 }, *res = NULL;
    hints.ai_family = AF_UNSPEC;
    hints.ai_socktype = SOCK_STREAM;
    hints.ai_flags = AI_ADDRCONFIG | AI_NUMERICHOST;
    int r = getaddrinfo("192.0.2.1", "80", &hints, &res);
    printf("getaddrinfo(AI_ADDRCONFIG, IPv4 literal): %s\n", r ? gai_strerror(r) : "ok");
    if (r) fail = 1;
    else freeaddrinfo(res);
    return fail;
}
