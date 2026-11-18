// Compile the service entry point into this test TU to inject a model without a production bypass flag.
#define main enhance_program_main
#include "../enhance/main.cpp"
#undef main
#include <sys/wait.h>
#include <chrono>
#include <thread>
namespace {
class IdentityModel final : public SpectrumModel {
public:
    void reset() override {}
    Spectrum infer(const Spectrum& x) override { return x; }
};
void require(bool condition, const char* reason) { if (!condition) throw std::runtime_error(reason); }
int connect_to(const std::string& path) {
    int fd = socket(AF_UNIX, SOCK_SEQPACKET|SOCK_CLOEXEC, 0);
    sockaddr_un addr{}; addr.sun_family = AF_UNIX;
    std::memcpy(addr.sun_path, path.c_str(), path.size()+1);
    if (connect(fd, reinterpret_cast<sockaddr*>(&addr), sizeof(addr)) < 0) { close(fd); return -1; }
    timeval timeout{2, 0};
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout));
    return fd;
}
Json query(int fd, std::uint16_t command) {
    mozart_ctl_header_t h{MOZART_CTL_MAGIC, MOZART_CTL_VERSION, command, 99, 2};
    std::array<char, sizeof(h)+2> request{}; std::memcpy(request.data(), &h, sizeof(h));
    request[sizeof(h)] = '{'; request[sizeof(h)+1] = '}';
    require(send(fd, request.data(), request.size(), MSG_NOSIGNAL) == ssize_t(request.size()), "control send");
    std::array<char, 4096> reply{}; auto n = recv(fd, reply.data(), reply.size(), 0);
    require(n >= ssize_t(sizeof(h)), "control response blocked or absent");
    mozart_ctl_header_t got{}; std::memcpy(&got, reply.data(), sizeof(got));
    require(got.request_id == h.request_id && got.type == command && got.body_len == n-sizeof(h), "control envelope echo");
    auto body = Json::parse(reply.data()+sizeof(h), reply.data()+n);
    require(body.at("status") == "ok" && body.at("code") == "Ok", "control response status");
    return body.at("data");
}
mozart_enhance_request_t frame(std::uint32_t idx) {
    mozart_enhance_request_t request{}; request.magic = MOZART_INPUT_PACKET_MAGIC;
    request.frame.meta.frame_idx = idx; request.frame.meta.pts_ns = idx*20000000ull;
    return request;
}
void send_frame(int fd, const mozart_enhance_request_t& request) {
    require(send(fd, &request, sizeof(request), MSG_NOSIGNAL) == sizeof(request), "audio send");
}
mozart_enhance_reply_t receive(int fd) {
    mozart_enhance_reply_t reply{};
    require(recv(fd, &reply, sizeof(reply), 0) == sizeof(reply), "audio reply"); return reply;
}
}
int main() {
    char directory[] = "/tmp/mozart-enhance-test-XXXXXX";
    if (!mkdtemp(directory)) return 1;
    const std::string ctl = std::string(directory)+"/ctl", audio = std::string(directory)+"/audio";
    pid_t pid = fork();
    if (pid == 0) {
        std::signal(SIGTERM, stop);
        try { IdentityModel model; serve(ctl, audio, model); _exit(0); }
        catch (const std::exception& e) { std::cerr << e.what() << '\n'; _exit(1); }
    }
    if (pid < 0) return 1;
    int result = 0;
    try {
        const std::string dummy = std::string(directory)+"/model", input = std::string(directory)+"/input", alias = std::string(directory)+"/alias";
        const std::string contents = "tiny dummy model: must remain byte-identical";
        { std::ofstream out(dummy, std::ios::binary); out << contents; }
        { std::ofstream out(input, std::ios::binary); const float value = 0.5f; out.write(reinterpret_cast<const char*>(&value), sizeof(value)); }
        IdentityModel injected;
        for (int kind = 0; kind < 3; ++kind) {
            if (kind == 1) require(link(dummy.c_str(), alias.c_str()) == 0, "model hardlink");
            if (kind == 2) require(symlink(dummy.c_str(), alias.c_str()) == 0, "model symlink");
            bool rejected = false;
            try { offline(input, kind == 0 ? dummy : alias, dummy, injected); }
            catch (const std::runtime_error& e) { rejected = std::string(e.what()) == "model and output must differ"; }
            require(rejected, "offline model alias must be rejected");
            std::ifstream check(dummy, std::ios::binary);
            const std::string after((std::istreambuf_iterator<char>(check)), {});
            require(after == contents, "model bytes changed");
            if (kind) unlink(alias.c_str());
        }
        unlink(dummy.c_str()); unlink(input.c_str());
        int idle = -1;
        for (int i = 0; i < 100 && idle < 0; ++i) {
            idle = connect_to(ctl); if (idle < 0) std::this_thread::sleep_for(std::chrono::milliseconds(10));
        }
        require(idle >= 0, "listener startup");
        int control_fd = connect_to(ctl); require(control_fd >= 0, "second control connect");
        require(query(control_fd, MOZART_CTL_ENHANCE_HELLO).contains("version"), "hello version");
        bool locked = false;
        try { Listener duplicate(ctl); } catch (const std::runtime_error&) { locked = true; }
        require(locked, "duplicate listener must fail");
        int fd = connect_to(audio); require(fd >= 0, "audio connect");
        for (std::uint32_t i = 1; i <= 4; ++i) {
            send_frame(fd, frame(i)); auto reply = receive(fd);
            require(reply.ready == (i > MOZART_ENHANCE_PRIMING_FRAMES), "priming");
            if (reply.ready) require(reply.frame.meta.frame_idx == i-MOZART_ENHANCE_PRIMING_FRAMES, "source metadata");
        }
        send_frame(fd, frame(100)); require(!receive(fd).ready, "gap reset");
        auto invalid = frame(101); invalid.magic = 0; send_frame(fd, invalid);
        char byte{}; require(recv(fd, &byte, 1, 0) == 0, "bad magic closes epoch"); close(fd);
        auto status = query(control_fd, MOZART_CTL_ENHANCE_GET_STATUS);
        require(status.at("frames_in") == 5 && status.at("frames_out") == 2, "frame counters");
        require(status.at("algorithm_delay_samples") == 640 && status.at("last_error").is_string(), "status delay/error");
        fd = connect_to(audio); require(fd >= 0, "audio reconnect");
        send_frame(fd, frame(1)); require(!receive(fd).ready, "reconnect reset");
        invalid = frame(2); float nan = std::nanf(""); std::memcpy(invalid.frame.pcm, &nan, sizeof(nan));
        send_frame(fd, invalid); require(recv(fd, &byte, 1, 0) == 0, "nonfinite input closes epoch"); close(fd);
        for (float value : {-1.0001f, 1.0001f, -std::numeric_limits<float>::infinity(), std::numeric_limits<float>::infinity()}) {
            fd = connect_to(audio); require(fd >= 0, "range reconnect");
            invalid = frame(1); std::memcpy(invalid.frame.pcm, &value, sizeof(value));
            send_frame(fd, invalid); require(recv(fd, &byte, 1, 0) == 0, "out-of-range input closes epoch"); close(fd);
        }
        for (float value : {-1.0f, 1.0f}) {
            fd = connect_to(audio); require(fd >= 0, "boundary reconnect");
            auto valid = frame(1); std::memcpy(valid.frame.pcm, &value, sizeof(value));
            send_frame(fd, valid); require(!receive(fd).ready, "inclusive PCM boundary accepted"); close(fd);
            std::this_thread::sleep_for(std::chrono::milliseconds(10));
        }
        fd = connect_to(audio); require(fd >= 0, "audio reconnect after invalid PCM");
        require(send(fd, "x", 1, MSG_NOSIGNAL) == 1, "short send");
        require(recv(fd, &byte, 1, 0) == 0, "wrong size closes epoch"); close(fd);
        fd = connect_to(audio); require(fd >= 0, "backpressure connection");
        // Do not receive replies. The server must close rather than block the control plane.
        int flags = fcntl(fd, F_GETFL); fcntl(fd, F_SETFL, flags|O_NONBLOCK);
        bool closed = false;
        for (std::uint32_t i = 1; i < 10000 && !closed; ++i) {
            auto packet = frame(i);
            if (send(fd, &packet, sizeof(packet), MSG_NOSIGNAL) < 0) {
                if (errno == EPIPE || errno == ECONNRESET) closed = true;
                else { require(errno == EAGAIN || errno == EWOULDBLOCK, "unexpected backpressure error"); std::this_thread::sleep_for(std::chrono::milliseconds(1)); }
            }
        }
        require(closed, "backpressure must close audio session");
        status = query(control_fd, MOZART_CTL_ENHANCE_GET_STATUS);
        require(status.at("last_error").get<std::string>().find("backpressure") != std::string::npos, "backpressure reason must be queryable");
        close(fd);
        std::vector<int> slots{idle, control_fd};
        for (int i = 2; i < 16; ++i) {
            int c = connect_to(ctl); require(c >= 0, "fill control capacity");
            query(c, MOZART_CTL_ENHANCE_HELLO); slots.push_back(c);
        }
        int excess = connect_to(ctl); require(excess >= 0, "excess connect");
        require(recv(excess, &byte, 1, 0) == 0, "capacity must reject excess connection"); close(excess);
        require(query(control_fd, MOZART_CTL_ENHANCE_GET_STATUS).at("last_error") == "control connection capacity reached", "capacity reason");
        const auto start = std::chrono::steady_clock::now();
        std::this_thread::sleep_for(control_idle_timeout+std::chrono::milliseconds(300));
        for (int c : slots) {
            require(recv(c, &byte, 1, 0) == 0, "idle control slot not reclaimed"); close(c);
        }
        require(std::chrono::steady_clock::now()-start >= control_idle_timeout, "real production idle interval");
        int recovered = connect_to(ctl); require(recovered >= 0, "connect after idle reclaim");
        require(query(recovered, MOZART_CTL_ENHANCE_HELLO).contains("version"), "service after idle reclaim"); close(recovered);
    } catch (const std::exception& e) { std::cerr << e.what() << '\n'; result = 1; }
    kill(pid, SIGTERM); int child_status = 0; waitpid(pid, &child_status, 0);
    if (!WIFEXITED(child_status) || WEXITSTATUS(child_status)) result = 1;
    unlink((ctl+".lock").c_str()); unlink((audio+".lock").c_str()); rmdir(directory);
    return result;
}
