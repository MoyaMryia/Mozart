// 构建契约：编译标准、扩展开关与基础语言特性必须符合项目约定。
// 该用例在 configure 阶段检查标准，在运行阶段验证可执行文件可以启动。
#include <cstdio>
#include <string_view>

#if __cplusplus < 202002L
#error "Mozart 要求 C++20"
#endif

namespace {

constexpr std::string_view kName = "mozart-build-contract";
static_assert(kName.size() == 21, "契约标识长度变化时同步更新用例");

}  // namespace

int main() {
    std::printf("%.*s\n", static_cast<int>(kName.size()), kName.data());
    return 0;
}
