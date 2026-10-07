/* confdefs.h */
#define PACKAGE_NAME ""
#define PACKAGE_TARNAME ""
#define PACKAGE_VERSION ""
#define PACKAGE_STRING ""
#define PACKAGE_BUGREPORT ""
#define PACKAGE_URL ""
#define HAVE_STDIO_H 1
#define HAVE_STDLIB_H 1
#define HAVE_STRING_H 1
#define HAVE_INTTYPES_H 1
#define HAVE_STDINT_H 1
#define HAVE_STRINGS_H 1
#define HAVE_SYS_STAT_H 1
#define HAVE_SYS_TYPES_H 1
#define HAVE_UNISTD_H 1
#define HAVE_SYS_TIME_H 1
#define STDC_HEADERS 1
#define SIZEOF_SHORT 2
#define SIZEOF_INT 4
#define SIZEOF_LONG 8
#define SIZEOF_VOID_P 8
#define SIZEOF_LONG_LONG 8
#define EI_HIDE_REAL_ERRNO 1
#define HAVE_UNALIGNED_WORD_ACCESS 1
#define HAVE_SYS_WAIT_H 1
#define HAVE_ARPA_INET_H 1
#define HAVE_FCNTL_H 1
#define HAVE_LIMITS_H 1
#define HAVE_MALLOC_H 1
#define HAVE_NETDB_H 1
#define HAVE_NETINET_IN_H 1
#define HAVE_STDDEF_H 1
#define HAVE_STDLIB_H 1
#define HAVE_STRING_H 1
#define HAVE_SYS_PARAM_H 1
#define HAVE_SYS_SOCKET_H 1
#define HAVE_SYS_SELECT_H 1
#define HAVE_SYS_TIME_H 1
#define HAVE_UNISTD_H 1
#define HAVE_SYS_TYPES_H 1
#define HAVE_SYS_UIO_H 1
#define HAVE_SOCKLEN_T /**/
#define HAVE_ALLOCA_H 1
#define HAVE_ALLOCA 1
#define HAVE_DUP2 1
#define HAVE_GETHOSTBYADDR 1
#define HAVE_GETHOSTBYNAME 1
#define HAVE_GETHOSTBYADDR_R 1
#define HAVE_GETHOSTBYNAME_R 1
#define HAVE_GETHOSTNAME 1
#define HAVE_WRITEV 1
#define HAVE_GETTIMEOFDAY 1
#define HAVE_INET_NTOA 1
#define HAVE_MEMCHR 1
#define HAVE_MEMMOVE 1
#define HAVE_MEMSET 1
#define HAVE_SELECT 1
#define HAVE_SOCKET 1
#define HAVE_STRCHR 1
#define HAVE_STRERROR 1
#define HAVE_STRRCHR 1
#define HAVE_STRSTR 1
#define HAVE_UNAME 1
#define HAVE_SYSCONF 1
#define HAVE_PTHREAD_H 1
#define SIZEOF_SHORT 2
#define SIZEOF_INT 4
#define SIZEOF_LONG 8
#define SIZEOF_LONG_LONG 8
#define SIZEOF___INT128_T 16
#define ETHR_HAVE___sync_synchronize ~0
#define ETHR_HAVE___sync_add_and_fetch 12
#define ETHR_HAVE___sync_fetch_and_and 12
#define ETHR_HAVE___sync_fetch_and_or 12
#define ETHR_HAVE___sync_val_compare_and_swap 12
#define ETHR_HAVE___atomic_store_n 28
#define ETHR_HAVE___atomic_load_n 28
#define ETHR_HAVE___atomic_add_fetch 28
#define ETHR_HAVE___atomic_fetch_and 28
/* end confdefs.h.  */

int
main (void)
{
int x[(2*__atomic_always_lock_free(sizeof(__int128_t), 0))-1]; volatile __int128_t var; __int128_t res = __atomic_fetch_or(&var, (__int128_t) 0, __ATOMIC_RELAXED); res = __atomic_fetch_or(&var, (__int128_t) 0, __ATOMIC_ACQUIRE); res = __atomic_fetch_or(&var, (__int128_t) 0, __ATOMIC_RELEASE);
  ;
  return 0;
}
