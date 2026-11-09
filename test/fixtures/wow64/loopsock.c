/* test/wow64/loopsock: a loopback TCP pair made the way libcurl's
 * socketpair stand-in makes one on Windows (its threaded resolver's wake-up
 * pair): a listener on 127.0.0.1 port 0, its port read back, a connect to
 * it, a non-blocking accept, then the check that the accepted end's peer is
 * the connecting end, address and port (getpeername against getsockname),
 * and a write across.
 * Prints "loopsock ok". */
#include <winsock2.h>
#include <stdio.h>

int main( void )
{
    struct sockaddr_in a, me, peer;
    int len = sizeof(a), ml = sizeof(me), pl = sizeof(peer), reuse = 1, r;
    SOCKET l, s0, s1, socks[2], got[2] = {0};
    u_long nb = 1;
    WSADATA wd;

    setvbuf( stdout, NULL, _IONBF, 0 );
    if (WSAStartup( MAKEWORD(2, 2), &wd )) { printf( "loopsock: WSAStartup\n" ); return 1; }
    l = socket( AF_INET, SOCK_STREAM, IPPROTO_TCP );
    memset( &a, 0, sizeof(a) );
    a.sin_family = AF_INET;
    a.sin_addr.s_addr = htonl( INADDR_LOOPBACK );
    if (setsockopt( l, SOL_SOCKET, SO_REUSEADDR, (char *)&reuse, sizeof(reuse) ) ||
        bind( l, (struct sockaddr *)&a, sizeof(a) ) ||
        getsockname( l, (struct sockaddr *)&a, &len ) || listen( l, 1 ))
    { printf( "loopsock: listener: %d\n", WSAGetLastError() ); return 1; }
    printf( "loopsock: listening on port %d\n", ntohs( a.sin_port ) );
    s0 = socket( AF_INET, SOCK_STREAM, 0 );
    if (connect( s0, (struct sockaddr *)&a, sizeof(a) )) { printf( "loopsock: connect: %d\n", WSAGetLastError() ); return 1; }
    ioctlsocket( l, FIONBIO, &nb );
    if ((s1 = accept( l, NULL, NULL )) == INVALID_SOCKET) { printf( "loopsock: accept: %d\n", WSAGetLastError() ); return 1; }
    getsockname( s0, (struct sockaddr *)&me, &ml );
    getpeername( s1, (struct sockaddr *)&peer, &pl );
    printf( "loopsock: connecting end %s:%d,", inet_ntoa( me.sin_addr ), ntohs( me.sin_port ) );
    printf( " the accepted end's peer %s:%d\n", inet_ntoa( peer.sin_addr ), ntohs( peer.sin_port ) );
    if (me.sin_port != peer.sin_port || me.sin_addr.s_addr != peer.sin_addr.s_addr) { printf( "loopsock: the ends disagree\n" ); return 1; }
    socks[0] = s0; socks[1] = s1;
    if (send( s0, (char *)socks, sizeof(socks), 0 ) != sizeof(socks)) { printf( "loopsock: send: %d\n", WSAGetLastError() ); return 1; }
    r = recv( s1, (char *)got, sizeof(got), 0 );
    if (r != sizeof(got) || memcmp( got, socks, sizeof(got) )) { printf( "loopsock: recv %d\n", r ); return 1; }
    printf( "loopsock ok\n" );
    return 0;
}
