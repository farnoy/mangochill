# Results

I tried to make input processing branchless, but there's just too much work to execute unconditionally to make sense.

```
process/dualsense-chaos-evabs-evkey/branchless/128
                        time:   [51.660 µs 51.730 µs 51.811 µs]
                        thrpt:  [284.11 Melem/s 284.56 Melem/s 284.94 Melem/s]
                 change:
                        time:   [−0.1788% −0.0103% +0.1657%] (p = 0.91 > 0.05)
                        thrpt:  [−0.1654% +0.0103% +0.1791%]
                        No change in performance detected.

process/dualsense-chaos-evabs-evkey/default/128
                        time:   [14.402 µs 14.422 µs 14.446 µs]
                        thrpt:  [1.0190 Gelem/s 1.0206 Gelem/s 1.0221 Gelem/s]
                 change:
                        time:   [+0.2136% +0.8221% +1.5592%] (p = 0.03 < 0.05)
                        thrpt:  [−1.5353% −0.8154% −0.2131%]
                        Change within noise threshold.
```

Branchless:

```
               757      context-switches                 #     89.3 cs/sec  cs_per_second
                 1      cpu-migrations                   #      0.1 migrations/sec  migrations_per_second
                 5      page-faults                      #      0.6 faults/sec  page_faults_per_second
          8,476.62 msec task-clock                       #      0.9 CPUs  CPUs_utilized
       910,366,687      L1-dcache-load-misses            #      2.2 %  l1d_miss_rate            (42.89%)
           395,636      branch-misses                    #      0.0 %  branch_miss_rate         (42.88%)
     6,502,542,568      branches                         #    767.1 M/sec  branch_frequency     (42.92%)
    44,012,225,260      cpu-cycles                       #      5.2 GHz  cycles_frequency       (42.84%)
   206,755,645,125      instructions                     #      4.7 instructions  insn_per_cycle  (42.83%)
       640,297,974      stalled-cycles-frontend          #     0.01 frontend_cycles_idle        (42.82%)
       
PipelineL1:

   219,730,483,625      de_src_op_disp.all               #      2.0 %  bad_speculation          (49.98%)
    44,116,386,583      ls_not_halted_cyc                                                       (49.98%)
   214,351,198,437      ex_ret_ops                       #     81.0 %  retiring                 (49.98%)
                 0      de_no_dispatch_per_slot.smt_contention #      0.0 %  smt_contention           (50.02%)
    44,115,576,995      ls_not_halted_cyc                                                       (50.02%)
     5,543,991,665      de_no_dispatch_per_slot.no_ops_from_frontend #      2.1 %  frontend_bound           (50.02%)
    44,116,420,605      ls_not_halted_cyc                                                       (50.02%)
    39,403,115,284      de_no_dispatch_per_slot.backend_stalls #     14.9 %  backend_bound            (49.98%)
    44,117,209,499      ls_not_halted_cyc                                                       (49.98%)
```

Default:

```
               119      context-switches                 #     13.2 cs/sec  cs_per_second
                 0      cpu-migrations                   #      0.0 migrations/sec  migrations_per_second
                 5      page-faults                      #      0.6 faults/sec  page_faults_per_second
          9,007.28 msec task-clock                       #      0.9 CPUs  CPUs_utilized
     3,478,251,939      L1-dcache-load-misses            #      4.9 %  l1d_miss_rate            (42.86%)
           259,718      branch-misses                    #      0.0 %  branch_miss_rate         (42.86%)
    40,565,922,819      branches                         #   4503.7 M/sec  branch_frequency     (42.86%)
    46,852,995,041      cpu-cycles                       #      5.2 GHz  cycles_frequency       (42.87%)
   267,650,217,427      instructions                     #      5.7 instructions  insn_per_cycle  (42.85%)
       283,176,765      stalled-cycles-frontend          #     0.01 frontend_cycles_idle        (42.85%)
       
PipelineL1:

   243,198,918,856      de_src_op_disp.all               #      0.8 %  bad_speculation          (50.00%)
    46,107,263,964      ls_not_halted_cyc                                                       (50.00%)
   240,999,427,047      ex_ret_ops                       #     87.1 %  retiring                 (50.00%)
                 0      de_no_dispatch_per_slot.smt_contention #      0.0 %  smt_contention           (50.01%)
    46,095,672,809      ls_not_halted_cyc                                                       (50.01%)
     6,547,697,792      de_no_dispatch_per_slot.no_ops_from_frontend #      2.4 %  frontend_bound           (50.00%)
    46,103,099,826      ls_not_halted_cyc                                                       (50.00%)
    26,954,108,433      de_no_dispatch_per_slot.backend_stalls #      9.7 %  backend_bound            (49.99%)
    46,114,677,011      ls_not_halted_cyc                                                       (49.99%)

```


I tried to design an _interleaved_ benchmark set by swapping between batches of various inputs, much like the real server is expected to do.
Branch misses go up to 0.8% in the default variant, while branchless stays flat at 0.0%
This shrinks the throughput gap to 2x but still favors the default, branchful variant.
There's too much work that can be skipped by branching.

# Next steps

Instead of trying to do branchless wholesale, it could make sense to deploy it tactically.

# Data

Capture with [the script](./capture.sh).
Requires sudo.

```sh
$ ./capture.sh /dev/eventN data/FILE.bin.zstd

# inspect
$ xxd -e -c 24 -g 2 (zstdcat benches/data/FILE.bin.zstd | psub) | less
```

`<base>[-trackpad][-accelerometer][-evabs][-evkey].bin.zstd`
`FILE` tags that affect what specialization is used:

- `-trackpad`: pointer-like ABS input (for held-in-ABS filtering).
- `-accelerometer`: excluded
- `-evabs`: HAS_ABS=true
- `-evkey`: HAS_KEY=true

# Profiling

`-D 1000` to start recording after zstdcat and warmup.

```sh
# fish syntax
$ set (RUSTFLAGS="-C target-cpu=native" cargo build --bench scratch --profile profiling --features branchless --message-format=json \
  | jq -r 'select(.reason=="compiler-artifact" and (.target.kind | index("bench"))).executable' \
  | tail -n 1)
  
$ perf stat -D 1000 -d $artifact --bench process/1kHz-mouse-spinning-clicking/branchless/128 --warm-up-time 1 --profile-time 10

# topdown Zen 4
$ perf stat -D 1000 -M PipelineL1 $artifact --bench process/1kHz-mouse-spinning-clicking/branchless/128 --warm-up-time 1 --profile-time 10

$ perf record -o /tmp/perf.data \
    -e cpu-clock:u \
    -D 1000 \
    --call-graph dwarf \
    --mmap-pages 8M \
    $artifact \
    --bench process/interleaved/branchless/8 --warm-up-time 1 --profile-time 10
    
# attribute to source lines / instructions
$ perf annotate -i /tmp/perf.data --stdio -M intel -l \
    -s '<scratch::BenchProcessorImpl<true, true> as scratch::BenchProcessor>::process_events'

# perf mem
$ perf mem record -o /tmp/perf-mem.data -- \
    $artifact --bench process/interleaved/branchless/8 --warm-up-time 1 --profile-time 10
$ perf mem report -i /tmp/perf-mem.data --stdio --sort mem,symbol,dso

# IBS hotness
$ perf annotate -i /tmp/ibs/perf-raw.data --stdio -M intel -l \
    -s '<scratch::BenchProcessorImpl<true> as scratch::BenchProcessor>::process_events'
$ perf annotate -i /tmp/ibs/perf-raw.data --stdio -M intel -l \
    -s '<scratch::BenchProcessorImpl<false> as scratch::BenchProcessor>::process_events'
```
