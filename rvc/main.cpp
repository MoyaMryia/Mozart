#include <cstdio>
#include "mozart/frame.h"

int main() {
    static_assert(sizeof(mozart_input_frame_t) == 1296);
    static_assert(sizeof(mozart_output_frame_t) == 3856);
    std::printf("mozart-rvc: placeholder\n");
    return 0;
}
