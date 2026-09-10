// Read-only macOS process counters; compile with clang native-process.c -o /tmp/native-process.
#include <libproc.h>
#include <mach/mach_time.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/proc_info.h>
#include <sys/resource.h>

int main(int argc, char **argv) {
    if (argc != 2) return 2;
    char *end;
    long value = strtol(argv[1], &end, 10);
    if (*end || value <= 0 || value > 2147483647) return 2;
    int pid = (int)value;
    struct proc_bsdinfo info;
    struct rusage_info_v2 usage;
    mach_timebase_info_data_t timebase;
    if (mach_timebase_info(&timebase) != KERN_SUCCESS) return 1;
    if (proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, &info, sizeof(info)) != sizeof(info)
        || proc_pid_rusage(pid, RUSAGE_INFO_V2, (rusage_info_t *)&usage)) return 1;
    printf("{\"pid\":%d,\"startEpochMs\":%.3f,\"cpuNs\":%llu,\"physicalFootprintBytes\":%llu}\n",
        pid, info.pbi_start_tvsec * 1000.0 + info.pbi_start_tvusec / 1000.0,
        (unsigned long long)((usage.ri_user_time + usage.ri_system_time) * timebase.numer / timebase.denom),
        (unsigned long long)usage.ri_phys_footprint);
    return 0;
}
