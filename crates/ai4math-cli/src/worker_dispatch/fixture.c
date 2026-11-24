#include <stdio.h>
#include <string.h>
#include <unistd.h>

int main(void) {
    char input[16384] = {0};
    size_t n = fread(input, 1, sizeof input - 1, stdin);
    if (n == 0 || n == sizeof input - 1 || strstr(input, "state.sqlite") || strstr(input, "fence")) return 31;
    if (strstr(input, "\"task_id\":\"wait\"")) sleep(1);
    if (strstr(input, "\"mode\":\"review_header\"")) {
        if (strstr(input, "WRITER-PRIVATE-CONTEXT") || strstr(input, "def helper")) return 32;
        puts("{\"kind\":\"review\",\"verdict\":\"insufficient_context\",\"rationale\":\"No dependency context\"}");
    } else if (strstr(input, "\"mode\":\"explore\"")) {
        if (strstr(input, "{{PROOF_BODY}}")) return 33;
        puts("{\"kind\":\"findings\",\"summary\":\"The trivial constructor might work\"}");
    } else {
        if (!strstr(input, "def helper")) return 34;
        puts("{\"kind\":\"proof_body\",\"proof_body\":\"True.intro\"}");
    }
    return 0;
}
