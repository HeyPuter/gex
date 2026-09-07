#include <errno.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

/* Whether a log line is the suite's own GL call trace rather than something a
   test chose to say.
 *
 * Several cases log every GL call they make -- "glGetError();", "// data =
 * { 16 }", "glBindBuffer(GL_ARRAY_BUFFER, 0);" -- and at sixteen attributes a
 * time that is hundreds of lines. They filled the detail ring and pushed out
 * the one message that mattered, so a failing case reported nothing but its
 * own trace. KHR-GL44.vertex_attrib_binding.basic-state4 was undiagnosable
 * for exactly this reason: it says "glVertexAttribDivisor state change check
 * failed" and none of it reached the terminal.
 *
 * Recognised by shape: a result line starts with `//`, and a call line starts
 * with `gl` plus an upper-case letter **and contains a parenthesis**. That
 * last clause is not decoration -- several tests name the call they are
 * complaining about, and "glVertexAttribDivisor state change check failed."
 * begins exactly like a trace line. Dropping it was how this filter hid the
 * message it was written to reveal. A call has arguments; prose does not. */
static int gl_trace_line(const char *text)
{
    if (!text)
        return 1;
    while (*text == ' ' || *text == '\t')
        ++text;
    if (text[0] == '/' && text[1] == '/')
        return 1;
    if (text[0] == 'g' && text[1] == 'l' && text[2] >= 'A' && text[2] <= 'Z') {
        for (const char *p = text; *p; ++p)
            if (*p == '(')
                return 1;
    }
    return 0;
}

int main(int argc, char **argv)
{
    char **child_argv = calloc((size_t)argc + 1, sizeof(*child_argv));
    if (!child_argv) return 125;
    const char *child_path = "/fs/glcts";
    int child_argc = 1;
    for (int i = 1; i < argc; ++i) {
        static const char prefix[] = "--gex-glcts=";
        if (strncmp(argv[i], prefix, sizeof(prefix) - 1) == 0) {
            child_path = argv[i] + sizeof(prefix) - 1;
            continue;
        }
        child_argv[child_argc++] = argv[i];
    }
    child_argv[0] = (char *)child_path;

    pid_t child = fork();
    if (child == 0) {
        execv(child_argv[0], child_argv);
        perror("execv glcts");
        _exit(126);
    }
    if (child < 0) {
        perror("fork");
        return 125;
    }

    int status = 0;
    if (waitpid(child, &status, 0) < 0) {
        perror("waitpid");
        return 125;
    }

    FILE *log = fopen("/tmp/glcts.qpa", "rb");
    if (!log) {
        fprintf(stderr, "open /tmp/glcts.qpa: errno %d\n", errno);
    } else {
        char *line = NULL;
        size_t capacity = 0;
        char current[512] = "";
        /* Aggregated rather than streamed. The dump runs to tens of thousands
           of lines for a large family and the console pipe keeps only the
           tail, which silently turns "here is what failed" into "here are the
           last few things that failed" -- a sample that looks like a census.
           Counting distinct messages fits, and is complete. */
        struct part {
            char name[256];
            unsigned long fail, pass;
        } *tally = NULL;
        size_t ntally = 0, tcap = 0;
        struct entry {
            char text[160];
            /* First three and most recent three. The first three alone are
               not a sample of anything -- they are whatever ran first -- and
               reasoning from them has produced wrong diagnoses more than once
               here. Spanning the run at least shows whether a message is
               confined to one kind of case or spread across all of them. */
            /* The full first message, before the key was trimmed. Grouping
               cuts at the colon so that per-value messages collapse into one
               row, which also throws away the value -- and for a message like
               "...with sizedFormat: 0x8229" the value is the whole
               diagnosis. */
            /* Up to three *distinct* full messages per census row. One was
               not enough: the key is cut at the colon so that per-value
               messages collapse into one row, and for a family whose every
               failure reads "Test fail: <topology>" that collapsed all of
               them onto the first topology and hid which six of eleven had
               actually failed. */
            char sample[3][512];
            unsigned samples;
            char example[3][256];
            char recent[3][256];
            unsigned examples;
            unsigned long count;
        } *seen = NULL;
        size_t nseen = 0, cap = 0;
        /* dEQP's own "Passed: n/m" summary goes to the console, and the
           harness captures a reflowing terminal that drops whole chunks -- it
           lost that line on a 1578-case family, which turns "how many failed"
           into a guess. Counted here from the QPA log, which is complete, and
           printed twice so at least one copy survives the capture. */
        unsigned long total_pass = 0, total_fail = 0, total_skip = 0;
        /* The messages the current case logged, kept until its result is
           known.
           
           dEQP's <Result> body is a one-word verdict for most families --
           "Fail", and nothing else -- while the sentence that says *what*
           mismatched is a <Text> the case wrote earlier. Those were being
           folded into the sweep-wide census by kind and never attached to the
           case that failed, so a silent failure in a browser reported "Fail"
           with its reason discarded. Four browser-only cases each cost
           several probe cycles to characterise for want of this, and one of
           them has had seven hypotheses eliminated without the CTS's own
           answer ever being read.
           
           A ring, and printed only for a failure, because these are the
           majority of the log's volume and a passing sweep must stay
           readable. */
        enum { DETAIL_KEEP = 6, DETAIL_CHARS = 240 };
        static char detail[DETAIL_KEEP][DETAIL_CHARS];
        unsigned ndetail = 0;
        puts("QPA-BEGIN");
        while (getline(&line, &capacity, log) >= 0) {
            static const char marker[] = "#beginTestCaseResult ";
            if (strncmp(line, marker, sizeof(marker) - 1) == 0) {
                char *path = line + sizeof(marker) - 1;
                char *nl = strchr(path, '\n');
                if (nl) *nl = '\0';
                snprintf(current, sizeof(current), "%s", path);
                ndetail = 0;
                continue;
            }
            char *status = strstr(line, "StatusCode=\"");
            if (status && current[0]) {
                status += 12;
                /* NotSupported is a conformant result, not a failure: dEQP
                   counts it in numNotSupported and a mustpass run passes with
                   it. Lumping it in with Fail sends the reader after features
                   the suite does not require. */
                const int unsupported =
                    strncmp(status, "NotSupported", 12) == 0;
                const int failed = !unsupported &&
                                   strncmp(status, "Pass", 4) != 0;
                /* And *why*. dEQP puts the reason in the body of the Result
                   element -- <Result StatusCode="Fail">reason</Result> --
                   while every other message it writes goes in <Text>. Reading
                   only <Text> meant a failing case reported the last thing
                   that went right: transform_feedback.api_errors_test failed
                   in a browser and its only line was "Linking failed as
                   expected", which describes a check that passed. One line
                   per failure, so a sweep stays readable. */
                if (failed) {
                    char *body = strchr(status, '>');
                    if (body) {
                        ++body;
                        char *shut = strstr(body, "</Result>");
                        if (shut) *shut = '\0';
                        for (char *c = body; *c; ++c)
                            if (*c == '\n' || *c == '\r') *c = ' ';
                        while (*body == ' ') ++body;
                        if (*body) printf("QPA-WHY    %s: %s\n", current, body);
                    }
                    /* And what the case itself said, oldest first. This is
                       the line that names the mismatched field. */
                    const unsigned kept = ndetail < DETAIL_KEEP
                        ? ndetail : DETAIL_KEEP;
                    const unsigned first = ndetail < DETAIL_KEEP
                        ? 0 : ndetail % DETAIL_KEEP;
                    for (unsigned k = 0; k < kept; ++k)
                        printf("QPA-SAID   %s: %s\n", current,
                               detail[(first + k) % DETAIL_KEEP]);
                }
                /* Tally both halves of the name separately. */
                char split[512];
                snprintf(split, sizeof(split), "%s", current);
                char *leaf = strrchr(split, '.');
                leaf = leaf ? leaf + 1 : split;
                char *sep = strstr(leaf, "_format_");
                char halves[2][256];
                int nhalves = 0;
                if (sep) {
                    *sep = '\0';
                    snprintf(halves[nhalves++], 256, "internal:%s", leaf);
                    snprintf(halves[nhalves++], 256, "readback:%s", sep + 8);
                } else {
                    snprintf(halves[nhalves++], 256, "case:%s", leaf);
                }
                for (int h = 0; h < nhalves; ++h) {
                    size_t j = 0;
                    for (; j < ntally; ++j)
                        if (strcmp(tally[j].name, halves[h]) == 0) break;
                    if (j == ntally) {
                        if (ntally == tcap) {
                            size_t grown = tcap ? tcap * 2 : 128;
                            struct part *bigger =
                                realloc(tally, grown * sizeof(*tally));
                            if (!bigger) break;
                            tally = bigger;
                            tcap = grown;
                        }
                        memset(&tally[j], 0, sizeof(tally[j]));
                        snprintf(tally[j].name, sizeof(tally[j].name), "%s",
                                 halves[h]);
                        ntally++;
                    }
                    if (failed) tally[j].fail++; else tally[j].pass++;
                }
                if (unsupported) total_skip++;
                else if (failed) total_fail++;
                else total_pass++;
                continue;
            }
            char *text = strstr(line, "<Text>");
            if (!text) continue;
            text += 6;
            char *end = strstr(text, "</Text>");
            static char joined[1024];
            if (end) {
                *end = '\0';
            } else {
                /* A message that spans lines. The CTS writes newlines inside
                   <Text> and this read only the first of them, which threw
                   away the half that says which thing failed:
                   primitive_restart logs "Test fail:" and then the topology
                   on the next line, so all seven of its sub-tests looked like
                   one message and none of them named itself. Newlines become
                   spaces so the census key stays a single line. */
                size_t at = 0;
                for (const char *c = text; *c && at + 1 < sizeof(joined); ++c)
                    joined[at++] = (*c == '\n' || *c == '\r') ? ' ' : *c;
                joined[at] = '\0';
                while (at + 1 < sizeof(joined)) {
                    if (getline(&line, &capacity, log) < 0) break;
                    char *stop = strstr(line, "</Text>");
                    if (stop) *stop = '\0';
                    joined[at++] = ' ';
                    for (const char *c = line; *c && at + 1 < sizeof(joined); ++c)
                        joined[at++] = (*c == '\n' || *c == '\r') ? ' ' : *c;
                    joined[at] = '\0';
                    if (stop) break;
                }
                while (at && joined[at - 1] == ' ') joined[--at] = '\0';
                text = joined;
            }

            /* The suite's own GL call trace is not a message and is not
               counted. Several cases log every call they make, and at sixteen
               attributes a time that buries everything a test actually said:
               the census for basic-state4 was nothing but "glGetError();" and
               "// data = { 0 }", and one glMemoryBarrier line with every
               barrier bit spelled out took four rows of it. The census exists
               to fit in a terminal, so what it spends its rows on decides
               whether it is any use. */
            if (gl_trace_line(text))
                continue;

            /* Group by the message's *kind*: the CTS appends the format
               combination to almost every one, which would make every message
               distinct and put the census back over the terminal's capacity.
               The harness captures a screen buffer rather than a pipe -- it
               reflows, and lines are simply lost -- so a census only means
               anything if it is small enough to fit. */
            /* Into the ring first: the census below truncates the message at
               its first colon to group by kind, and the part after the colon
               is the half that carries the numbers. */
            if (current[0] && !gl_trace_line(text)) {
                snprintf(detail[ndetail % DETAIL_KEEP], DETAIL_CHARS, "%s",
                         text);
                ++ndetail;
            }
            const char *full = text;
            char key[160];
            snprintf(key, sizeof(key), "%s", text);
            char *cut = strstr(key, " for input");
            if (!cut) cut = strchr(key, ':');
            if (cut) *cut = '\0';
            text = key;

            size_t i = 0;
            for (; i < nseen; ++i)
                if (strncmp(seen[i].text, text, sizeof(seen[i].text) - 1) == 0)
                    break;
            if (i == nseen) {
                if (nseen == cap) {
                    size_t grown = cap ? cap * 2 : 64;
                    struct entry *bigger = realloc(seen, grown * sizeof(*seen));
                    if (!bigger) break;
                    seen = bigger;
                    cap = grown;
                }
                memset(&seen[i], 0, sizeof(seen[i]));
                snprintf(seen[i].text, sizeof(seen[i].text), "%s", text);
                nseen++;
            }
            seen[i].count++;
            {
                unsigned known = 0;
                for (; known < seen[i].samples; ++known)
                    if (strcmp(seen[i].sample[known], full) == 0)
                        break;
                if (known == seen[i].samples && seen[i].samples < 3)
                    snprintf(seen[i].sample[seen[i].samples++],
                             sizeof(seen[i].sample[0]), "%s", full);
            }
            if (current[0]) {
                if (seen[i].examples < 3)
                    snprintf(seen[i].example[seen[i].examples++],
                             sizeof(seen[i].example[0]), "%s", current);
                /* Rolling: after the run these hold the last three. */
                snprintf(seen[i].recent[seen[i].count % 3],
                         sizeof(seen[i].recent[0]), "%s", current);
            }
        }
        /* Which *combination* fails. A packed_pixels case is named
           <internalformat>_format_<readback>, and the interesting question is
           always which half is responsible -- tallied here rather than by
           pairing case names with results in the captured output, because
           that capture drops lines and a lossy tally reads exactly like a
           complete one. */
        /* Packed onto one line per category, worst first. The capture is a
           terminal: it reflows and drops whole chunks, and a hundred-line
           table comes back as a dozen arbitrary rows that still look like a
           table. Two lines survive; a table does not. */
        for (int pass_kind = 0; pass_kind < 2; ++pass_kind) {
            const char *prefix = pass_kind ? "readback:" : "internal:";
            const size_t plen = strlen(prefix);
            char out[1024];
            int used = snprintf(out, sizeof(out), "QPA-TALLY %s",
                                pass_kind ? "readback" : "internal");
            for (int rank = 0; rank < 8; ++rank) {
                size_t best = ntally;
                for (size_t i = 0; i < ntally; ++i) {
                    if (strncmp(tally[i].name, prefix, plen) != 0) continue;
                    if (!tally[i].fail || tally[i].pass == ULONG_MAX) continue;
                    if (best == ntally || tally[i].fail > tally[best].fail)
                        best = i;
                }
                if (best == ntally) break;
                int n = snprintf(out + used, sizeof(out) - (size_t)used,
                                 " %s=%lu/%lu", tally[best].name + plen,
                                 tally[best].fail,
                                 tally[best].fail + tally[best].pass);
                if (n > 0 && (size_t)(used + n) < sizeof(out)) used += n;
                tally[best].pass = ULONG_MAX;   /* consumed */
            }
            puts(out);
        }
        printf("QPA-TOTAL pass=%lu fail=%lu skip=%lu\n",
               total_pass, total_fail, total_skip);
        free(tally);
        /* Largest first, and only the top few. Printed in insertion order the
           list ran past the terminal's scrollback and the capture kept
           whichever groups happened to be last -- which on
           packed_pixels.varied_rectangle hid every message the varied storage
           modes produce behind the four the plain rectangle test also
           produces, and made the two families look identical. */
        printf("QPA-KINDS %zu\n", nseen);
        const unsigned show = nseen < 8 ? (unsigned)nseen : 8u;
        for (unsigned rank = 0; rank < show; ++rank) {
            size_t best = nseen;
            for (size_t i = 0; i < nseen; ++i) {
                if (seen[i].count == ULONG_MAX) continue;
                if (best == nseen || seen[i].count > seen[best].count)
                    best = i;
            }
            if (best == nseen) break;
            const size_t i = best;
            printf("QPA %lu x %s\n", seen[i].count, seen[i].text);
            /* The harness reads a real xterm buffer, and a line longer than
               the terminal is wrapped there and its tail is lost -- which
               silently cut the output format off every message of the form
               "... for input = [X, Y] output = [Z, W]", leaving only the
               half that was already known. Emit long samples in terminal-
               sized pieces so the whole message survives the capture. */
            /* Always, not only when the key was trimmed: the census line
               itself is subject to the same terminal cut, so a long message
               with nothing to trim was still losing its tail. */
            for (unsigned k = 0; k < seen[i].samples; ++k) {
                const char *s = seen[i].sample[k];
                size_t left = strlen(s);
                int first = 1;
                while (left) {
                    const size_t take = left > 68 ? 68 : left;
                    printf("%s %.*s\n", first ? "QPA-FULL  " : "QPA-CONT  ",
                           (int)take, s);
                    s += take;
                    left -= take;
                    first = 0;
                }
            }
            if (seen[i].examples)
                printf("QPA-CASE   %s\n", seen[i].example[0]);
            for (unsigned e = 0; e < 3; ++e)
                if (seen[i].recent[e][0] &&
                    strcmp(seen[i].recent[e], seen[i].example[0]) != 0) {
                    printf("QPA-LATE   %s\n", seen[i].recent[e]);
                    break;
                }
            seen[i].count = ULONG_MAX;   /* consumed */
        }
        free(seen);
        free(line);
        printf("QPA-TOTAL pass=%lu fail=%lu skip=%lu\n",
               total_pass, total_fail, total_skip);
        puts("QPA-END");
        fclose(log);
    }

    if (WIFEXITED(status)) return WEXITSTATUS(status);
    if (WIFSIGNALED(status)) return 128 + WTERMSIG(status);
    return 125;
}
