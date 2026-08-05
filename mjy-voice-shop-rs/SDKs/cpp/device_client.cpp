#include <arpa/inet.h>
#include <netdb.h>
#include <poll.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

#include <algorithm>
#include <cerrno>
#include <chrono>
#include <cctype>
#include <csignal>
#include <condition_variable>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <deque>
#include <fstream>
#include <functional>
#include <iostream>
#include <map>
#include <mutex>
#include <random>
#include <set>
#include <sstream>
#include <stdexcept>
#include <string>
#include <thread>
#include <vector>

namespace {

struct Args {
    std::string host = "127.0.0.1";
    int port = 8787;
    std::string device_id = "DOLL-0001";
    std::string device_secret;
    std::string base_path;
    std::string text;
    std::string audio_path;
    std::string output = "/tmp/mjy-cpp-device-reply.mp3";
    std::string in_format = "mp3";
    int in_rate = 16000;
    std::string out_format = "mp3";
    int out_rate = 16000;
    bool play = false;
    std::string play_cmd;
    bool self_test = false;
    bool interrupt_after_first_chunk = false;
};

std::string path_join(const std::string& base_path, const std::string& path) {
    if (base_path.empty() || base_path == "/") return path;
    if (base_path.back() == '/') return base_path.substr(0, base_path.size() - 1) + path;
    return base_path + path;
}

std::string base64_encode(const std::vector<uint8_t>& input) {
    static const char* table = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    std::string output;
    int value = 0;
    int bits = -6;
    for (uint8_t byte : input) {
        value = (value << 8) + byte;
        bits += 8;
        while (bits >= 0) {
            output.push_back(table[(value >> bits) & 0x3f]);
            bits -= 6;
        }
    }
    if (bits > -6) output.push_back(table[((value << 8) >> (bits + 8)) & 0x3f]);
    while (output.size() % 4) output.push_back('=');
    return output;
}

std::vector<uint8_t> base64_decode(const std::string& input) {
    if (input.size() % 4 != 0) throw std::runtime_error("invalid base64 length");
    static const std::string table = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    std::vector<int> map(256, -1);
    for (int i = 0; i < 64; ++i) map[static_cast<unsigned char>(table[i])] = i;
    std::vector<uint8_t> output;
    int value = 0;
    int bits = -8;
    bool padding = false;
    for (size_t index = 0; index < input.size(); ++index) {
        unsigned char ch = input[index];
        if (ch == '=') {
            padding = true;
            if (index < input.size() - 2) throw std::runtime_error("invalid base64 padding");
            continue;
        }
        if (padding || map[ch] == -1) throw std::runtime_error("invalid base64 character");
        value = (value << 6) + map[ch];
        bits += 6;
        if (bits >= 0) {
            output.push_back(static_cast<uint8_t>((value >> bits) & 0xff));
            bits -= 8;
        }
    }
    return output;
}

int connect_tcp(const std::string& host, int port) {
    addrinfo hints{};
    hints.ai_family = AF_UNSPEC;
    hints.ai_socktype = SOCK_STREAM;
    addrinfo* result = nullptr;
    const std::string port_text = std::to_string(port);
    if (getaddrinfo(host.c_str(), port_text.c_str(), &hints, &result) != 0) {
        throw std::runtime_error("getaddrinfo failed");
    }
    int fd = -1;
    for (addrinfo* item = result; item; item = item->ai_next) {
        fd = socket(item->ai_family, item->ai_socktype, item->ai_protocol);
        if (fd < 0) continue;
        if (connect(fd, item->ai_addr, item->ai_addrlen) == 0) break;
        close(fd);
        fd = -1;
    }
    freeaddrinfo(result);
    if (fd < 0) throw std::runtime_error("connect failed");
    return fd;
}

template <typename SendFn>
void send_all_bytes_with(const uint8_t* data, size_t size, SendFn send_fn) {
    size_t offset = 0;
    while (offset < size) {
        const ssize_t sent = send_fn(data + offset, size - offset);
        if (sent > 0) {
            offset += static_cast<size_t>(sent);
            continue;
        }
        if (sent < 0 && errno == EINTR) continue;
        throw std::runtime_error(
            std::string("send failed: ") + (sent == 0 ? "connection closed" : std::strerror(errno)));
    }
}

ssize_t socket_send_no_sigpipe(int fd, const uint8_t* data, size_t size) {
#if defined(__APPLE__) && defined(SO_NOSIGPIPE)
    const int enabled = 1;
    if (setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &enabled, sizeof(enabled)) != 0) {
        return -1;
    }
    return ::send(fd, data, size, 0);
#elif defined(MSG_NOSIGNAL)
    return ::send(fd, data, size, MSG_NOSIGNAL);
#else
    std::signal(SIGPIPE, SIG_IGN);
    return ::send(fd, data, size, 0);
#endif
}

void send_all(int fd, const std::string& data) {
    send_all_bytes_with(
        reinterpret_cast<const uint8_t*>(data.data()), data.size(),
        [fd](const uint8_t* bytes, size_t size) {
            return socket_send_no_sigpipe(fd, bytes, size);
        });
}

void send_all(int fd, const std::vector<uint8_t>& data) {
    send_all_bytes_with(
        data.data(), data.size(),
        [fd](const uint8_t* bytes, size_t size) {
            return socket_send_no_sigpipe(fd, bytes, size);
        });
}

std::string url_encode(const std::string& value) {
    static const char* hex = "0123456789ABCDEF";
    std::string encoded;
    for (unsigned char ch : value) {
        if (std::isalnum(ch) || ch == '-' || ch == '_' || ch == '.' || ch == '~') encoded.push_back(ch);
        else {
            encoded.push_back('%');
            encoded.push_back(hex[ch >> 4]);
            encoded.push_back(hex[ch & 0x0f]);
        }
    }
    return encoded;
}

std::string read_until(int fd, const std::string& marker) {
    std::string data;
    char buffer[1024];
    while (data.find(marker) == std::string::npos) {
        ssize_t n = recv(fd, buffer, sizeof(buffer), 0);
        if (n <= 0) throw std::runtime_error("connection closed");
        data.append(buffer, buffer + n);
    }
    return data;
}

std::string json_escape(const std::string& value) {
    std::string out;
    for (char ch : value) {
        if (ch == '"' || ch == '\\') {
            out.push_back('\\');
            out.push_back(ch);
        } else if (ch == '\n') {
            out += "\\n";
        } else {
            out.push_back(ch);
        }
    }
    return out;
}

std::string extract_json_string(const std::string& json, const std::string& key) {
    const std::string marker = "\"" + key + "\":\"";
    size_t start = json.find(marker);
    if (start == std::string::npos) return "";
    start += marker.size();
    std::string value;
    bool escaping = false;
    for (size_t i = start; i < json.size(); ++i) {
        char ch = json[i];
        if (escaping) {
            value.push_back(ch);
            escaping = false;
        } else if (ch == '\\') {
            escaping = true;
        } else if (ch == '"') {
            break;
        } else {
            value.push_back(ch);
        }
    }
    return value;
}

std::string extract_json_value(const std::string& json, const std::string& key) {
    const std::string marker = "\"" + key + "\":";
    size_t start = json.find(marker);
    if (start == std::string::npos) return "";
    start += marker.size();
    while (start < json.size() && json[start] == ' ') ++start;
    size_t end = start;
    while (end < json.size() && json[end] != ',' && json[end] != '}' && json[end] != ']') ++end;
    return json.substr(start, end - start);
}

std::string http_post_auth(const Args& args) {
    int fd = connect_tcp(args.host, args.port);
    std::string body = "{\"device_id\":\"" + json_escape(args.device_id) +
                       "\",\"device_secret\":\"" + json_escape(args.device_secret) + "\"}";
    std::ostringstream request;
    request << "POST " << path_join(args.base_path, "/api/device/auth") << " HTTP/1.1\r\n"
            << "Host: " << args.host << ":" << args.port << "\r\n"
            << "Content-Type: application/json\r\n"
            << "Content-Length: " << body.size() << "\r\n"
            << "Connection: close\r\n\r\n"
            << body;
    send_all(fd, request.str());
    std::string response;
    char buffer[1024];
    while (true) {
        ssize_t n = recv(fd, buffer, sizeof(buffer), 0);
        if (n <= 0) break;
        response.append(buffer, buffer + n);
    }
    close(fd);
    const std::string token = extract_json_string(response, "token");
    if (token.empty()) throw std::runtime_error("auth failed: token not found");
    return token;
}

std::vector<uint8_t> read_file(const std::string& path) {
    std::ifstream file(path, std::ios::binary);
    if (!file) throw std::runtime_error("cannot open file: " + path);
    return std::vector<uint8_t>((std::istreambuf_iterator<char>(file)), {});
}

void append_file(const std::string& path, const std::vector<uint8_t>& bytes) {
    std::ofstream file(path, std::ios::binary | std::ios::app);
    file.write(reinterpret_cast<const char*>(bytes.data()), static_cast<std::streamsize>(bytes.size()));
}

bool process_group_exists(pid_t pgid) {
    if (pgid <= 0) return false;
    if (kill(-pgid, 0) == 0) return true;
    return errno == EPERM;
}

class ProcessGroupReaper {
   public:
    static ProcessGroupReaper& instance() {
        static ProcessGroupReaper* reaper = new ProcessGroupReaper();
        return *reaper;
    }

    bool handoff(pid_t leader_pid, pid_t pgid) {
        {
            std::lock_guard<std::mutex> lock(mutex_);
            ++pending_;
        }
        try {
            std::thread([this, leader_pid, pgid]() {
                reap_owned_group(leader_pid, pgid);
                {
                    std::lock_guard<std::mutex> lock(mutex_);
                    --pending_;
                }
                idle_.notify_all();
            }).detach();
        } catch (...) {
            std::lock_guard<std::mutex> lock(mutex_);
            --pending_;
            idle_.notify_all();
            return false;
        }
        return true;
    }

    bool wait_for_idle(std::chrono::milliseconds timeout) {
        std::unique_lock<std::mutex> lock(mutex_);
        return idle_.wait_for(lock, timeout, [this]() { return pending_ == 0; });
    }

   private:
    static void kill_group(pid_t pgid) {
        if (pgid <= 0) return;
        while (kill(-pgid, SIGKILL) < 0 && errno == EINTR) {}
    }

    static void reap_owned_group(pid_t leader_pid, pid_t pgid) {
        kill_group(pgid);
        if (leader_pid > 0) {
            while (true) {
                const pid_t result = waitpid(leader_pid, nullptr, 0);
                if (result == leader_pid || (result < 0 && errno == ECHILD)) break;
                if (result < 0 && errno == EINTR) continue;
                break;
            }
        }
        while (process_group_exists(pgid)) {
            kill_group(pgid);
            std::this_thread::sleep_for(std::chrono::milliseconds(10));
        }
    }

    std::mutex mutex_;
    std::condition_variable idle_;
    size_t pending_ = 0;
};

class StreamPlayer {
   public:
    explicit StreamPlayer(bool force_reaper_handoff_for_test = false)
        : force_reaper_handoff_for_test_(force_reaper_handoff_for_test) {}

    ~StreamPlayer() {
        try {
            close();
        } catch (const std::exception& error) {
            std::cerr << "stream_player_cleanup_failed=" << error.what() << std::endl;
        }
    }

    void start(const std::string& command) {
        if (active()) {
            stop_now();
            if (active()) {
                throw std::runtime_error("previous stream player process group is still active");
            }
        }
        int pipe_fds[2];
        if (pipe(pipe_fds) != 0) {
            std::cerr << "stream_player_unavailable command=\"" << command << "\"" << std::endl;
            return;
        }
        const pid_t child = fork();
        if (child < 0) {
            ::close(pipe_fds[0]);
            ::close(pipe_fds[1]);
            std::cerr << "stream_player_unavailable command=\"" << command << "\"" << std::endl;
            return;
        }
        if (child == 0) {
            ::close(pipe_fds[1]);
            setpgid(0, 0);
            if (dup2(pipe_fds[0], STDIN_FILENO) < 0) _exit(126);
            ::close(pipe_fds[0]);
            execl("/bin/sh", "sh", "-c", command.c_str(), static_cast<char*>(nullptr));
            _exit(127);
        }
        ::close(pipe_fds[0]);
        setpgid(child, child);
        if (getpgid(child) != child) {
            ::close(pipe_fds[1]);
            kill(child, SIGKILL);
            while (waitpid(child, nullptr, 0) < 0 && errno == EINTR) {}
            throw std::runtime_error("stream player process group setup failed");
        }
        input_fd_ = pipe_fds[1];
        leader_pid_ = child;
        pgid_ = child;
        std::signal(SIGPIPE, SIG_IGN);
        std::cout << "stream_player=" << command << std::endl;
    }

    void write(const std::vector<uint8_t>& bytes) {
        if (input_fd_ < 0 || bytes.empty()) return;
        size_t offset = 0;
        while (offset < bytes.size()) {
            const ssize_t written = ::write(
                input_fd_, bytes.data() + offset, bytes.size() - offset);
            if (written > 0) {
                offset += static_cast<size_t>(written);
                continue;
            }
            if (written < 0 && errno == EINTR) continue;
            close_input();
            return;
        }
    }

    void close() {
        close_input();
        refresh_finished_group();
        if (!active()) return;
        if (wait_for_graceful_group_exit(std::chrono::milliseconds(1000))) return;
        signal_group(SIGTERM);
        if (wait_for_group_exit(std::chrono::milliseconds(200))) return;
        force_kill_and_reap();
    }

    void stop_now() {
        close_input();
        refresh_finished_group();
        if (!active()) return;
        force_kill_and_reap();
    }

    bool active() const {
        return leader_pid_ > 0 || process_group_exists(pgid_);
    }

    pid_t process_group_id() const { return pgid_; }

   private:
    void close_input() {
        if (input_fd_ < 0) return;
        ::close(input_fd_);
        input_fd_ = -1;
    }

    bool reap_leader_nonblocking() {
        if (leader_pid_ <= 0) return true;
        int status = 0;
        const pid_t result = waitpid(leader_pid_, &status, WNOHANG);
        if (result == leader_pid_ || (result < 0 && errno == ECHILD)) {
            leader_pid_ = -1;
            return true;
        }
        if (result < 0 && errno != EINTR) {
            throw std::runtime_error("stream player waitpid failed");
        }
        return false;
    }

    void refresh_finished_group() {
        reap_leader_nonblocking();
        if (leader_pid_ <= 0 && !process_group_exists(pgid_)) pgid_ = -1;
    }

    bool wait_for_graceful_group_exit(std::chrono::milliseconds timeout) {
        const auto deadline = std::chrono::steady_clock::now() + timeout;
        while (true) {
            const bool leader_finished = reap_leader_nonblocking();
            if (!process_group_exists(pgid_)) {
                if (leader_finished) {
                    pgid_ = -1;
                    return true;
                }
            }
            if (leader_finished && process_group_exists(pgid_)) return false;
            if (std::chrono::steady_clock::now() >= deadline) return false;
            std::this_thread::sleep_for(std::chrono::milliseconds(5));
        }
    }

    bool wait_for_group_exit(std::chrono::milliseconds timeout) {
        const auto deadline = std::chrono::steady_clock::now() + timeout;
        while (true) {
            reap_leader_nonblocking();
            if (!process_group_exists(pgid_)) {
                pgid_ = -1;
                return leader_pid_ <= 0;
            }
            if (std::chrono::steady_clock::now() >= deadline) return false;
            std::this_thread::sleep_for(std::chrono::milliseconds(5));
        }
    }

    void signal_group(int signal_number) {
        if (pgid_ > 0) kill(-pgid_, signal_number);
    }

    void force_kill_and_reap() {
        const pid_t process_group = pgid_;
        const bool group_signaled =
            pgid_ > 0 && (kill(-pgid_, SIGKILL) == 0 || errno == ESRCH);
        if (!group_signaled && leader_pid_ > 0) kill(leader_pid_, SIGKILL);
        if (!force_reaper_handoff_for_test_ &&
            wait_for_group_exit(std::chrono::milliseconds(100))) {
            return;
        }
        if (!ProcessGroupReaper::instance().handoff(leader_pid_, pgid_)) {
            signal_group(SIGKILL);
            throw std::runtime_error(
                "stream player reaper handoff failed: pgid=" + std::to_string(process_group));
        }
        leader_pid_ = -1;
        pgid_ = -1;
    }

    int input_fd_ = -1;
    pid_t leader_pid_ = -1;
    pid_t pgid_ = -1;
    bool force_reaper_handoff_for_test_ = false;
};

void self_test_stream_player_stop_is_bounded_and_restartable() {
    const pid_t test_pid = fork();
    if (test_pid < 0) throw std::runtime_error("stream player self-test fork failed");
    if (test_pid == 0) {
        setpgid(0, 0);
        StreamPlayer player;
        player.start("trap '' TERM; while :; do :; done");
        const auto started = std::chrono::steady_clock::now();
        player.stop_now();
        const auto elapsed = std::chrono::duration_cast<std::chrono::milliseconds>(
            std::chrono::steady_clock::now() - started);
        if (elapsed > std::chrono::milliseconds(500) || player.active()) _exit(2);
        player.start("cat >/dev/null");
        if (!player.active()) _exit(3);
        player.write({'o', 'k'});
        player.close();
        _exit(player.active() ? 4 : 0);
    }
    setpgid(test_pid, test_pid);
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::milliseconds(1200);
    int status = 0;
    while (std::chrono::steady_clock::now() < deadline) {
        const pid_t result = waitpid(test_pid, &status, WNOHANG);
        if (result == test_pid) {
            if (WIFEXITED(status) && WEXITSTATUS(status) == 0) return;
            throw std::runtime_error("stream player stop/restart self-test failed");
        }
        if (result < 0 && errno != EINTR) {
            throw std::runtime_error("stream player self-test wait failed");
        }
        std::this_thread::sleep_for(std::chrono::milliseconds(10));
    }
    kill(-test_pid, SIGKILL);
    while (waitpid(test_pid, &status, 0) < 0 && errno == EINTR) {}
    throw std::runtime_error("stream player stop_now exceeded 1200ms");
}

void self_test_stream_player_cleans_background_descendants() {
    StreamPlayer player;
    player.start("sleep 30 &");
    const pid_t graceful_pgid = player.process_group_id();
    std::this_thread::sleep_for(std::chrono::milliseconds(50));
    player.close();
    if (graceful_pgid <= 0 || process_group_exists(graceful_pgid) || player.active()) {
        throw std::runtime_error("graceful close leaked background player descendant");
    }

    player.start("trap '' TERM; while :; do :; done");
    const pid_t timeout_pgid = player.process_group_id();
    const auto close_started = std::chrono::steady_clock::now();
    player.close();
    const auto close_elapsed = std::chrono::duration_cast<std::chrono::milliseconds>(
        std::chrono::steady_clock::now() - close_started);
    if (timeout_pgid <= 0 || process_group_exists(timeout_pgid) || player.active() ||
        close_elapsed > std::chrono::milliseconds(1700)) {
        throw std::runtime_error("timed-out graceful close did not converge");
    }
    player.start("cat >/dev/null");
    if (!player.active()) throw std::runtime_error("player restart after close timeout failed");
    player.close();

    player.start("(trap '' TERM; while :; do :; done) &");
    const pid_t interrupted_pgid = player.process_group_id();
    std::this_thread::sleep_for(std::chrono::milliseconds(50));
    const auto started = std::chrono::steady_clock::now();
    player.stop_now();
    const auto elapsed = std::chrono::duration_cast<std::chrono::milliseconds>(
        std::chrono::steady_clock::now() - started);
    if (interrupted_pgid <= 0 || process_group_exists(interrupted_pgid) ||
        player.active() || elapsed > std::chrono::milliseconds(500)) {
        throw std::runtime_error("stop_now leaked background player descendant");
    }

    player.start("cat >/dev/null");
    if (!player.active()) throw std::runtime_error("player restart after descendant cleanup failed");
    player.close();
}

void self_test_forced_reaper_handoff_is_bounded_and_restartable() {
    int pgid_pipe[2];
    if (pipe(pgid_pipe) != 0) throw std::runtime_error("reaper self-test pipe failed");
    const pid_t test_pid = fork();
    if (test_pid < 0) throw std::runtime_error("reaper self-test fork failed");
    if (test_pid == 0) {
        ::close(pgid_pipe[0]);
        setpgid(0, 0);
        StreamPlayer player(true);
        player.start("(trap '' TERM; while :; do :; done) &");
        const pid_t player_pgid = player.process_group_id();
        if (::write(pgid_pipe[1], &player_pgid, sizeof(player_pgid)) !=
            static_cast<ssize_t>(sizeof(player_pgid))) {
            _exit(2);
        }
        ::close(pgid_pipe[1]);
        const auto started = std::chrono::steady_clock::now();
        player.stop_now();
        const auto elapsed = std::chrono::duration_cast<std::chrono::milliseconds>(
            std::chrono::steady_clock::now() - started);
        if (elapsed > std::chrono::milliseconds(300) || player.active()) _exit(3);
        player.start("cat >/dev/null");
        if (!player.active()) _exit(4);
        player.close();
        if (!ProcessGroupReaper::instance().wait_for_idle(std::chrono::milliseconds(1000))) {
            _exit(5);
        }
        _exit(process_group_exists(player_pgid) ? 6 : 0);
    }
    ::close(pgid_pipe[1]);
    pollfd descriptor{pgid_pipe[0], POLLIN, 0};
    int poll_result;
    do {
        poll_result = poll(&descriptor, 1, 500);
    } while (poll_result < 0 && errno == EINTR);
    if (poll_result <= 0 || (descriptor.revents & POLLIN) == 0) {
        ::close(pgid_pipe[0]);
        kill(test_pid, SIGKILL);
        while (waitpid(test_pid, nullptr, 0) < 0 && errno == EINTR) {}
        throw std::runtime_error("reaper self-test player pgid watchdog expired");
    }
    pid_t player_pgid = -1;
    const ssize_t read_size = ::read(pgid_pipe[0], &player_pgid, sizeof(player_pgid));
    ::close(pgid_pipe[0]);
    if (read_size != static_cast<ssize_t>(sizeof(player_pgid)) || player_pgid <= 0) {
        kill(test_pid, SIGKILL);
        while (waitpid(test_pid, nullptr, 0) < 0 && errno == EINTR) {}
        throw std::runtime_error("reaper self-test did not report player pgid");
    }
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::milliseconds(1500);
    int status = 0;
    while (std::chrono::steady_clock::now() < deadline) {
        const pid_t result = waitpid(test_pid, &status, WNOHANG);
        if (result == test_pid) {
            if (WIFEXITED(status) && WEXITSTATUS(status) == 0) return;
            kill(-player_pgid, SIGKILL);
            throw std::runtime_error("forced reaper handoff self-test failed");
        }
        if (result < 0 && errno != EINTR) {
            kill(-player_pgid, SIGKILL);
            throw std::runtime_error("reaper self-test waitpid failed");
        }
        std::this_thread::sleep_for(std::chrono::milliseconds(10));
    }
    kill(-player_pgid, SIGKILL);
    kill(test_pid, SIGKILL);
    while (waitpid(test_pid, &status, 0) < 0 && errno == EINTR) {}
    throw std::runtime_error("forced reaper handoff exceeded watchdog deadline");
}

void self_test_closed_socket_does_not_raise_sigpipe() {
    int sockets[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sockets) != 0) {
        throw std::runtime_error("SIGPIPE self-test socketpair failed");
    }
    const pid_t test_pid = fork();
    if (test_pid < 0) {
        close(sockets[0]);
        close(sockets[1]);
        throw std::runtime_error("SIGPIPE self-test fork failed");
    }
    if (test_pid == 0) {
        close(sockets[1]);
        std::signal(SIGPIPE, SIG_DFL);
        try {
            send_all(sockets[0], std::string("closed"));
        } catch (...) {
            _exit(0);
        }
        _exit(2);
    }
    close(sockets[0]);
    close(sockets[1]);
    int status = 0;
    while (waitpid(test_pid, &status, 0) < 0 && errno == EINTR) {}
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        throw std::runtime_error("closed socket raised SIGPIPE");
    }
}

void self_test_send_retries_eintr() {
    const std::string expected = "retry";
    std::string received;
    int calls = 0;
    send_all_bytes_with(
        reinterpret_cast<const uint8_t*>(expected.data()), expected.size(),
        [&received, &calls](const uint8_t* data, size_t size) -> ssize_t {
            ++calls;
            if (calls == 1) {
                errno = EINTR;
                return -1;
            }
            received.append(reinterpret_cast<const char*>(data), size);
            return static_cast<ssize_t>(size);
        });
    if (calls != 2 || received != expected) {
        throw std::runtime_error("send EINTR retry self-test failed");
    }
}

std::string random_ws_key() {
    std::vector<uint8_t> bytes(16);
    std::random_device rd;
    for (auto& byte : bytes) byte = static_cast<uint8_t>(rd());
    return base64_encode(bytes);
}

std::string websocket_path(const Args& args, const std::string& token) {
    std::ostringstream request;
    request << path_join(args.base_path, "/api/device/voice")
            << "?device_id=" << url_encode(args.device_id)
            << "&token=" << url_encode(token)
            << "&in_format=" << args.in_format
            << "&in_rate=" << args.in_rate
            << "&out_format=" << args.out_format
            << "&out_rate=" << args.out_rate;
    return request.str();
}

void websocket_handshake(int fd, const Args& args, const std::string& token) {
    std::ostringstream request;
    request << "GET " << websocket_path(args, token) << " HTTP/1.1\r\n"
            << "Host: " << args.host << ":" << args.port << "\r\n"
            << "Upgrade: websocket\r\n"
            << "Connection: Upgrade\r\n"
            << "Sec-WebSocket-Version: 13\r\n"
            << "Sec-WebSocket-Key: " << random_ws_key() << "\r\n\r\n";
    send_all(fd, request.str());
    std::string response = read_until(fd, "\r\n\r\n");
    if (response.find(" 101 ") == std::string::npos) {
        throw std::runtime_error("websocket upgrade failed: " + response.substr(0, response.find("\r\n")));
    }
}

void send_ws_text(int fd, const std::string& text) {
    std::vector<uint8_t> frame;
    frame.push_back(0x81);
    const size_t len = text.size();
    if (len < 126) {
        frame.push_back(static_cast<uint8_t>(0x80 | len));
    } else if (len <= 65535) {
        frame.push_back(0x80 | 126);
        frame.push_back(static_cast<uint8_t>((len >> 8) & 0xff));
        frame.push_back(static_cast<uint8_t>(len & 0xff));
    } else {
        frame.push_back(0x80 | 127);
        for (int shift = 56; shift >= 0; shift -= 8) {
            frame.push_back(static_cast<uint8_t>((len >> shift) & 0xff));
        }
    }
    uint8_t mask[4] = {0x12, 0x34, 0x56, 0x78};
    frame.insert(frame.end(), mask, mask + 4);
    for (size_t i = 0; i < len; ++i) {
        frame.push_back(static_cast<uint8_t>(text[i]) ^ mask[i % 4]);
    }
    send_all(fd, frame);
}

bool recv_exact(int fd, uint8_t* data, size_t len) {
    size_t got = 0;
    while (got < len) {
        ssize_t n = recv(fd, data + got, len - got, 0);
        if (n <= 0) return false;
        got += static_cast<size_t>(n);
    }
    return true;
}

std::string recv_ws_text(int fd) {
    uint8_t header[2];
    if (!recv_exact(fd, header, 2)) return "";
    uint8_t opcode = header[0] & 0x0f;
    uint64_t len = header[1] & 0x7f;
    if (len == 126) {
        uint8_t ext[2];
        if (!recv_exact(fd, ext, 2)) return "";
        len = (static_cast<uint64_t>(ext[0]) << 8) | ext[1];
    } else if (len == 127) {
        uint8_t ext[8];
        if (!recv_exact(fd, ext, 8)) return "";
        len = 0;
        for (uint8_t byte : ext) len = (len << 8) | byte;
    }
    std::vector<uint8_t> payload(len);
    if (len && !recv_exact(fd, payload.data(), static_cast<size_t>(len))) return "";
    if (opcode == 0x8) return "";
    return std::string(payload.begin(), payload.end());
}

void send_text_turn(int fd, const std::string& conversation_id, const std::string& text) {
    send_ws_text(fd, "{\"type\":\"text\",\"conversation_id\":\"" + json_escape(conversation_id) +
                     "\",\"text\":\"" + json_escape(text) + "\"}");
}

std::pair<size_t, int> audio_frame_config(const Args& args, const std::vector<uint8_t>& audio) {
    if (audio.empty()) throw std::runtime_error("audio input file is empty");
    if (args.in_format == "opus") {
        throw std::runtime_error(
            "Opus file upload is unsupported: a flat file does not preserve variable packet boundaries; "
            "send one complete device-encoded packet per audio_stream_chunk");
    }
    size_t frame_bytes = 4096;
    int frame_duration_ms = 40;
    if (args.in_format == "pcm") {
        if (audio.size() % 2 != 0) {
            throw std::runtime_error("PCM input must contain complete signed 16-bit little-endian samples");
        }
        frame_bytes = static_cast<size_t>(args.in_rate * 2 * 40 / 1000);
    } else if (args.in_format == "speex") {
        frame_bytes = args.in_rate == 8000 ? 38 : 60;
        frame_duration_ms = 20;
        if (audio.size() % frame_bytes != 0) {
            throw std::runtime_error("Speex input must contain whole quality-7 packets of " +
                                     std::to_string(frame_bytes) + " bytes");
        }
    }
    return {frame_bytes, frame_duration_ms};
}

void send_audio_stream(int fd, const Args& args, const std::string& conversation_id,
                       const std::vector<uint8_t>& audio) {
    auto frame_config = audio_frame_config(args, audio);
    const size_t frame_bytes = frame_config.first;
    const int frame_duration_ms = frame_config.second;
    const auto now = std::chrono::duration_cast<std::chrono::milliseconds>(
                         std::chrono::system_clock::now().time_since_epoch())
                         .count();
    send_ws_text(fd, "{\"type\":\"audio_stream_start\",\"conversation_id\":\"" + conversation_id +
                     "\",\"trace_id\":\"cpp-" + std::to_string(now) +
                     "\",\"client_sent_ms\":" + std::to_string(now) + "}");
    for (size_t offset = 0; offset < audio.size(); offset += frame_bytes) {
        size_t end = std::min(offset + frame_bytes, audio.size());
        std::vector<uint8_t> chunk(audio.begin() + static_cast<long>(offset), audio.begin() + static_cast<long>(end));
        send_ws_text(fd, "{\"type\":\"audio_stream_chunk\",\"audio\":\"" + base64_encode(chunk) + "\"}");
        std::this_thread::sleep_for(std::chrono::milliseconds(frame_duration_ms));
    }
    send_ws_text(fd, "{\"type\":\"audio_stream_end\",\"conversation_id\":\"" + conversation_id + "\"}");
}

Args parse_args(int argc, char** argv) {
    Args args;
    for (int i = 1; i < argc; ++i) {
        std::string key = argv[i];
        auto next = [&]() -> std::string {
            if (i + 1 >= argc) throw std::runtime_error("missing value for " + key);
            return argv[++i];
        };
        if (key == "--host") args.host = next();
        else if (key == "--port") args.port = std::stoi(next());
        else if (key == "--device-id") args.device_id = next();
        else if (key == "--device-secret") args.device_secret = next();
        else if (key == "--base-path") args.base_path = next();
        else if (key == "--text") args.text = next();
        else if (key == "--audio") args.audio_path = next();
        else if (key == "--in-format") args.in_format = next();
        else if (key == "--in-rate") args.in_rate = std::stoi(next());
        else if (key == "--out-format") args.out_format = next();
        else if (key == "--out-rate") args.out_rate = std::stoi(next());
        else if (key == "--output") args.output = next();
        else if (key == "--play") args.play = true;
        else if (key == "--play-cmd") args.play_cmd = next();
        else if (key == "--self-test") args.self_test = true;
        else if (key == "--interrupt-after-first-chunk") args.interrupt_after_first_chunk = true;
        else throw std::runtime_error("unknown arg: " + key);
    }
    const std::vector<std::string> formats = {"mp3", "pcm", "opus", "speex"};
    if (std::find(formats.begin(), formats.end(), args.in_format) == formats.end()) {
        throw std::runtime_error("--in-format must be mp3, pcm, opus, or speex");
    }
    if (std::find(formats.begin(), formats.end(), args.out_format) == formats.end()) {
        throw std::runtime_error("--out-format must be mp3, pcm, opus, or speex");
    }
    const auto valid_rate = [](int rate) { return rate == 8000 || rate == 16000 || rate == 24000; };
    if (!valid_rate(args.in_rate)) throw std::runtime_error("--in-rate must be 8000, 16000, or 24000");
    if (!valid_rate(args.out_rate)) throw std::runtime_error("--out-rate must be 8000, 16000, or 24000");
    if ((args.in_format == "speex" && args.in_rate == 24000) ||
        (args.out_format == "speex" && args.out_rate == 24000)) {
        throw std::runtime_error("Speex only supports 8000 or 16000 Hz");
    }
    return args;
}

bool is_loopback_host(std::string host) {
    std::transform(host.begin(), host.end(), host.begin(), [](unsigned char ch) {
        return static_cast<char>(std::tolower(ch));
    });
    return host == "localhost" || host == "127.0.0.1" || host == "::1";
}

void resolve_device_secret(Args& args) {
    const bool local = is_loopback_host(args.host);
    if (args.device_id == "DOLL-0001" && !local) {
        throw std::runtime_error(
            "DOLL-0001 is local-only; provision a separate device and pass --device-secret");
    }
    if (!args.device_secret.empty()) return;
    if (local && args.device_id == "DOLL-0001") {
        args.device_secret = "demo-secret";
        return;
    }
    throw std::runtime_error(
        "--device-secret is required for non-local or independently provisioned devices");
}

std::string default_play_command(const std::string& format, int sample_rate) {
    if (format == "pcm") {
        return "ffplay -nodisp -autoexit -loglevel quiet -f s16le -ar " +
               std::to_string(sample_rate) + " -ac 1 -i pipe:0";
    }
    if (format == "mp3") return "mpg123 -q -";
    throw std::runtime_error("--play is unsupported for raw " + format +
                             " packets; save them or pass each packet to the device decoder");
}

std::string output_path_for_format(const std::string& path, const std::string& format) {
    const std::string suffix = format == "opus" ? ".opuspack" : "." + format;
    const size_t slash = path.find_last_of('/');
    const size_t dot = path.find_last_of('.');
    const size_t basename_start = slash == std::string::npos ? 0 : slash + 1;
    if (dot != std::string::npos && dot > basename_start) {
        return path.substr(0, dot) + suffix;
    }
    return path + suffix;
}

class TtsSequenceValidator {
   public:
    void validate(const std::string& message) {
        const std::string seq_text = extract_json_value(message, "seq");
        const std::string last_text = extract_json_value(message, "is_last");
        if (seq_text.empty() || (last_text != "true" && last_text != "false")) {
            throw std::runtime_error("invalid TTS seq/is_last");
        }
        int seq = std::stoi(seq_text);
        if (seq < 0 || closed_.count(seq)) {
            throw std::runtime_error("TTS sequence out of order");
        }
        if (last_text == "true") closed_.insert(seq);
    }
   private:
    std::set<int> closed_;
};

class TtsOrderedAudio {
   public:
    std::vector<std::vector<uint8_t>> accept(const std::string& message,
                                              const std::vector<uint8_t>& bytes) {
        const int seq = std::stoi(extract_json_value(message, "seq"));
        const bool is_last = extract_json_value(message, "is_last") == "true";
        if (closed_.count(seq)) throw std::runtime_error("TTS sequence out of order");
        seen_.insert(seq);
        std::vector<std::vector<uint8_t>> ready;
        if (seq == next_seq_) {
            if (!bytes.empty()) ready.push_back(bytes);
        } else if (!bytes.empty()) {
            buffered_[seq].push_back(bytes);
        }
        if (is_last) closed_.insert(seq);
        while (closed_.count(next_seq_)) {
            ++next_seq_;
            auto found = buffered_.find(next_seq_);
            if (found != buffered_.end()) {
                ready.insert(ready.end(), found->second.begin(), found->second.end());
                buffered_.erase(found);
            }
        }
        return ready;
    }

    void finish() const {
        if (!buffered_.empty() || seen_ != closed_) {
            throw std::runtime_error("TTS sequence incomplete at voice_done");
        }
    }

   private:
    int next_seq_ = 0;
    std::map<int, std::vector<std::vector<uint8_t>>> buffered_;
    std::set<int> closed_;
    std::set<int> seen_;
};

class PlaybackState {
   public:
    static constexpr size_t kInterruptedTurnLimit = 64;

    void observe(const std::string& message) {
        const std::string event_type = extract_json_string(message, "event_type");
        const std::string conversation_id = extract_json_string(message, "conversation_id");
        const std::string turn_id = extract_json_string(message, "turn_id");
        if (event_type == "tts_audio_chunk") {
            if (conversation_id.empty() || turn_id.empty() || should_drop(message)) return;
            if (conversation_id != conversation_id_ || turn_id != turn_id_) {
                reset_playback_buffers();
                conversation_id_ = conversation_id;
                turn_id_ = turn_id;
            }
            return;
        }
        if (event_type != "tts_interrupted" && event_type != "voice_done" &&
            event_type != "conversation_ended") {
            return;
        }
        if (conversation_id != conversation_id_ || turn_id != turn_id_) return;
        clear_active_playback();
    }

    bool should_drop(const std::string& message) const {
        const std::string event_type = extract_json_string(message, "event_type");
        if (event_type != "llm_delta" && event_type != "reply_sentence" &&
            event_type != "tts_audio_chunk" && event_type != "voice_done") {
            return false;
        }
        const TurnKey key = {
            extract_json_string(message, "conversation_id"),
            extract_json_string(message, "turn_id"),
        };
        return !key.first.empty() && !key.second.empty() && interrupted_.count(key) != 0;
    }

    std::string interrupt_payload() {
        if (conversation_id_.empty() || turn_id_.empty()) return "";
        const TurnKey key = {conversation_id_, turn_id_};
        if (interrupted_.count(key)) return "";
        interrupted_.insert(key);
        interrupted_order_.push_back(key);
        while (interrupted_order_.size() > kInterruptedTurnLimit) {
            interrupted_.erase(interrupted_order_.front());
            interrupted_order_.pop_front();
        }
        const std::string payload =
            "{\"type\":\"tts_interrupt\",\"conversation_id\":\"" +
            json_escape(conversation_id_) + "\",\"turn_id\":\"" + json_escape(turn_id_) +
            "\",\"source\":\"button\"}";
        clear_active_playback();
        return payload;
    }

    const std::string& conversation_id() const { return conversation_id_; }
    const std::string& turn_id() const { return turn_id_; }
    TtsSequenceValidator& sequence() { return sequence_; }
    TtsOrderedAudio& ordered() { return ordered_; }

   private:
    using TurnKey = std::pair<std::string, std::string>;

    void reset_playback_buffers() {
        sequence_ = TtsSequenceValidator();
        ordered_ = TtsOrderedAudio();
    }

    void clear_active_playback() {
        conversation_id_.clear();
        turn_id_.clear();
        reset_playback_buffers();
    }

    std::string conversation_id_;
    std::string turn_id_;
    std::set<TurnKey> interrupted_;
    std::deque<TurnKey> interrupted_order_;
    TtsSequenceValidator sequence_;
    TtsOrderedAudio ordered_;
};

enum class ButtonInterruptResult {
    NoActiveTurn,
    Sent,
    SendFailed,
};

ButtonInterruptResult interrupt_tts_from_button(
    PlaybackState& playback,
    const std::function<void()>& stop_and_clear_playback,
    const std::function<void(const std::string&)>& send_control) {
    const std::string payload = playback.interrupt_payload();
    if (payload.empty()) return ButtonInterruptResult::NoActiveTurn;
    stop_and_clear_playback();
    try {
        send_control(payload);
    } catch (const std::exception& error) {
        std::cerr << "tts_interrupt_send_failed=" << error.what() << std::endl;
        return ButtonInterruptResult::SendFailed;
    }
    return ButtonInterruptResult::Sent;
}

class InterruptedOneShotCompletion {
   public:
    explicit InterruptedOneShotCompletion(std::string turn_id = "")
        : turn_id_(std::move(turn_id)) {}

    void begin(const std::string& turn_id) {
        turn_id_ = turn_id;
        acknowledged_ = false;
        business_tail_seen_ = false;
    }

    bool observe(const std::string& event_type, const std::string& turn_id) {
        if (turn_id.empty() || turn_id != turn_id_) return false;
        if (event_type == "tts_interrupted") acknowledged_ = true;
        if (event_type == "latency_metrics") business_tail_seen_ = true;
        return acknowledged_ && business_tail_seen_;
    }

   private:
    std::string turn_id_;
    bool acknowledged_ = false;
    bool business_tail_seen_ = false;
};

std::vector<uint8_t> encode_output_chunk(const std::string& format,
                                         const std::vector<uint8_t>& bytes) {
    if (format != "opus") return bytes;
    const uint32_t size = static_cast<uint32_t>(bytes.size());
    std::vector<uint8_t> framed = {
        static_cast<uint8_t>(size & 0xff), static_cast<uint8_t>((size >> 8) & 0xff),
        static_cast<uint8_t>((size >> 16) & 0xff), static_cast<uint8_t>((size >> 24) & 0xff),
    };
    framed.insert(framed.end(), bytes.begin(), bytes.end());
    return framed;
}

std::vector<uint8_t> validate_tts_message(const std::string& message, const Args& args,
                                          TtsSequenceValidator& sequence) {
    const std::string format = extract_json_string(message, "format");
    const std::string rate = extract_json_value(message, "sample_rate");
    const std::string channels = extract_json_value(message, "channels");
    if (format != args.out_format || rate != std::to_string(args.out_rate) || channels != "1") {
        throw std::runtime_error("tts_audio_chunk metadata mismatch");
    }
    if (args.out_format == "pcm" && extract_json_value(message, "bit_depth") != "16") {
        throw std::runtime_error("PCM bit_depth must be 16");
    }
    const std::string audio = extract_json_string(message, "audio");
    std::vector<uint8_t> decoded = base64_decode(audio);
    sequence.validate(message);
    return decoded;
}

void run_self_test() {
    self_test_stream_player_stop_is_bounded_and_restartable();
    self_test_stream_player_cleans_background_descendants();
    self_test_forced_reaper_handoff_is_bounded_and_restartable();
    self_test_closed_socket_does_not_raise_sigpipe();
    self_test_send_retries_eintr();
    char program[] = "device_client";
    char interrupt_flag[] = "--interrupt-after-first-chunk";
    char* interrupt_argv[] = {program, interrupt_flag};
    if (!parse_args(2, interrupt_argv).interrupt_after_first_chunk) {
        throw std::runtime_error("interrupt demo flag self-test failed");
    }
    Args local_demo;
    resolve_device_secret(local_demo);
    if (local_demo.device_secret != "demo-secret") {
        throw std::runtime_error("local demo credential self-test failed");
    }
    Args public_demo;
    public_demo.host = "example.test";
    bool public_demo_rejected = false;
    try { resolve_device_secret(public_demo); } catch (...) { public_demo_rejected = true; }
    if (!public_demo_rejected) throw std::runtime_error("public demo credential self-test failed");
    Args args;
    args.device_id = "DOLL A/1";
    args.in_format = "pcm";
    args.in_rate = 8000;
    args.out_format = "mp3";
    args.out_rate = 24000;
    const std::string path = websocket_path(args, "tok+en=?");
    if (path.find("device_id=DOLL%20A%2F1") == std::string::npos || path.find("token=tok%2Ben%3D%3F") == std::string::npos ||
        path.find("in_format=pcm&in_rate=8000&out_format=mp3&out_rate=24000") == std::string::npos) throw std::runtime_error("query self-test failed");
    bool bad64 = false;
    try { base64_decode("***="); } catch (...) { bad64 = true; }
    if (!bad64) throw std::runtime_error("strict base64 self-test failed");
    TtsSequenceValidator sequence;
    const std::string valid = "{\"event_type\":\"tts_audio_chunk\",\"payload\":{\"audio\":\"AA==\",\"format\":\"mp3\",\"sample_rate\":24000,\"channels\":1,\"seq\":0,\"is_last\":true}}";
    if (validate_tts_message(valid, args, sequence).size() != 1) throw std::runtime_error("TTS metadata self-test failed");
    bool order = false;
    try { validate_tts_message(valid, args, sequence); } catch (...) { order = true; }
    if (!order) throw std::runtime_error("TTS order self-test failed");
    TtsOrderedAudio ordered;
    auto event = [](int seq, bool last) {
        return "{\"seq\":" + std::to_string(seq) + ",\"is_last\":" +
               (last ? "true" : "false") + "}";
    };
    if (!ordered.accept(event(1, false), {'B'}).empty()) throw std::runtime_error("TTS ordering early release");
    if (ordered.accept(event(0, false), {'A'}) != std::vector<std::vector<uint8_t>>{{'A'}}) throw std::runtime_error("TTS seq0 streaming failed");
    if (!ordered.accept(event(1, true), {'b'}).empty()) throw std::runtime_error("TTS ordering early final release");
    const auto released = ordered.accept(event(0, true), {'a'});
    if (released != std::vector<std::vector<uint8_t>>{{'a'}, {'B'}, {'b'}}) throw std::runtime_error("TTS interleave ordering failed");
    ordered.finish();
    TtsOrderedAudio incomplete;
    incomplete.accept(event(2, true), {'x'});
    bool incomplete_failed = false;
    try { incomplete.finish(); } catch (...) { incomplete_failed = true; }
    if (!incomplete_failed) throw std::runtime_error("TTS incomplete self-test failed");
    const auto opuspack = encode_output_chunk("opus", {'a', 'b', 'c'});
    if (opuspack != std::vector<uint8_t>({3, 0, 0, 0, 'a', 'b', 'c'}) ||
        output_path_for_format("/tmp/a.b/reply.bin", "opus") != "/tmp/a.b/reply.opuspack" ||
        output_path_for_format("/tmp/.reply", "mp3") != "/tmp/.reply.mp3") {
        throw std::runtime_error("Opus packet framing self-test failed");
    }
    Args pcm = args;
    pcm.in_format = "pcm";
    bool odd_pcm = false;
    try { audio_frame_config(pcm, std::vector<uint8_t>(1)); } catch (...) { odd_pcm = true; }
    if (!odd_pcm) throw std::runtime_error("odd PCM self-test failed");
    Args speex = args;
    speex.in_format = "speex";
    speex.in_rate = 8000;
    bool residual = false;
    try { audio_frame_config(speex, std::vector<uint8_t>(39)); } catch (...) { residual = true; }
    if (!residual) throw std::runtime_error("Speex residual self-test failed");
    Args opus = args;
    opus.in_format = "opus";
    bool flat_opus = false;
    try { audio_frame_config(opus, std::vector<uint8_t>(20)); } catch (...) { flat_opus = true; }
    if (!flat_opus) throw std::runtime_error("Opus flat file self-test failed");

    PlaybackState playback;
    const std::string first_chunk =
        "{\"event_type\":\"tts_audio_chunk\",\"conversation_id\":\"conversation-1\","
        "\"turn_id\":\"turn-1\",\"payload\":{\"audio\":\"AA==\",\"format\":\"mp3\","
        "\"sample_rate\":24000,\"channels\":1,\"seq\":0,\"is_last\":false}}";
    playback.observe(first_chunk);
    playback.ordered().accept("{\"seq\":1,\"is_last\":false}", {'Q'});
    std::vector<std::string> button_order;
    const ButtonInterruptResult interrupted = interrupt_tts_from_button(
        playback,
        [&button_order, &playback]() {
            if (!playback.turn_id().empty()) {
                throw std::runtime_error("button did not clear active turn before player stop");
            }
            playback.ordered().finish();
            button_order.push_back("stop_and_clear");
        },
        [&button_order](const std::string& payload) {
            button_order.push_back(payload);
        });
    if (interrupted != ButtonInterruptResult::Sent ||
        button_order != std::vector<std::string>({
            "stop_and_clear",
            "{\"type\":\"tts_interrupt\",\"conversation_id\":\"conversation-1\","
            "\"turn_id\":\"turn-1\",\"source\":\"button\"}",
        })) {
        throw std::runtime_error("button interrupt payload/order self-test failed");
    }
    if (interrupt_tts_from_button(playback, []() {}, [](const std::string&) {}) !=
        ButtonInterruptResult::NoActiveTurn) {
        throw std::runtime_error("duplicate button interrupt self-test failed");
    }
    PlaybackState offline_playback;
    offline_playback.observe(
        "{\"event_type\":\"tts_audio_chunk\",\"conversation_id\":\"conversation-offline\","
        "\"turn_id\":\"turn-offline\",\"payload\":{}}");
    int offline_stop_count = 0;
    if (interrupt_tts_from_button(
            offline_playback,
            [&offline_stop_count]() { ++offline_stop_count; },
            [](const std::string&) { throw std::runtime_error("offline"); }) !=
            ButtonInterruptResult::SendFailed ||
        offline_stop_count != 1 || !offline_playback.turn_id().empty() ||
        !offline_playback.should_drop(
            "{\"event_type\":\"voice_done\",\"conversation_id\":\"conversation-offline\","
            "\"turn_id\":\"turn-offline\",\"payload\":{}}") ||
        interrupt_tts_from_button(offline_playback, []() {}, [](const std::string&) {}) !=
            ButtonInterruptResult::NoActiveTurn) {
        throw std::runtime_error("offline button interrupt self-test failed");
    }
    const std::string late_chunk =
        "{\"event_type\":\"tts_audio_chunk\",\"conversation_id\":\"conversation-1\","
        "\"turn_id\":\"turn-1\",\"payload\":{\"audio\":\"not-base64\",\"seq\":0}}";
    if (!playback.should_drop(late_chunk)) {
        throw std::runtime_error("late interrupted TTS self-test failed");
    }
    const std::string late_reply =
        "{\"event_type\":\"reply_sentence\",\"conversation_id\":\"conversation-1\","
        "\"turn_id\":\"turn-1\",\"payload\":{}}";
    const std::string late_done =
        "{\"event_type\":\"voice_done\",\"conversation_id\":\"conversation-1\","
        "\"turn_id\":\"turn-1\",\"payload\":{}}";
    if (!playback.should_drop(late_reply) || !playback.should_drop(late_done)) {
        throw std::runtime_error("late interrupted event self-test failed");
    }
    const std::string next_chunk =
        "{\"event_type\":\"tts_audio_chunk\",\"conversation_id\":\"conversation-1\","
        "\"turn_id\":\"turn-2\",\"payload\":{\"audio\":\"AA==\",\"format\":\"mp3\","
        "\"sample_rate\":24000,\"channels\":1,\"seq\":0,\"is_last\":true}}";
    if (playback.should_drop(next_chunk)) {
        throw std::runtime_error("next TTS turn self-test failed");
    }
    playback.observe(next_chunk);
    if (playback.turn_id() != "turn-2" || playback.conversation_id() != "conversation-1") {
        throw std::runtime_error("next TTS turn tracking self-test failed");
    }
    for (int index = 2; index <= 66; ++index) {
        const std::string turn_id = "turn-" + std::to_string(index);
        playback.observe(
            "{\"event_type\":\"tts_audio_chunk\",\"conversation_id\":\"conversation-1\","
            "\"turn_id\":\"" + turn_id + "\",\"payload\":{}}");
        if (playback.interrupt_payload().empty()) {
            throw std::runtime_error("bounded interrupted turn setup failed");
        }
    }
    if (playback.should_drop(late_chunk)) {
        throw std::runtime_error("old interrupted turn eviction self-test failed");
    }
    if (!playback.should_drop(
            "{\"event_type\":\"tts_audio_chunk\",\"conversation_id\":\"conversation-1\","
            "\"turn_id\":\"turn-66\",\"payload\":{}}")) {
        throw std::runtime_error("recent interrupted turn retention self-test failed");
    }
    InterruptedOneShotCompletion ack_first("turn-ack-first");
    if (ack_first.observe("tts_interrupted", "turn-ack-first") ||
        !ack_first.observe("latency_metrics", "turn-ack-first")) {
        throw std::runtime_error("interrupt ack-first completion self-test failed");
    }
    InterruptedOneShotCompletion tail_first("turn-tail-first");
    if (tail_first.observe("latency_metrics", "turn-tail-first") ||
        !tail_first.observe("tts_interrupted", "turn-tail-first") ||
        tail_first.observe("latency_metrics", "other-turn")) {
        throw std::runtime_error("interrupt tail-first completion self-test failed");
    }
    std::cout << "C++ SDK protocol self-test: PASS" << std::endl;
}

}  // namespace

int main(int argc, char** argv) {
    try {
        Args args = parse_args(argc, argv);
        if (args.self_test) {
            run_self_test();
            return 0;
        }
        resolve_device_secret(args);
        if (args.text.empty() == args.audio_path.empty()) {
            throw std::runtime_error("choose exactly one of --text or --audio");
        }
        args.output = output_path_for_format(args.output, args.out_format);
        std::vector<uint8_t> input_audio;
        if (!args.audio_path.empty()) {
            input_audio = read_file(args.audio_path);
            audio_frame_config(args, input_audio);
        }
        std::ofstream(args.output, std::ios::binary).close();
        std::string token = http_post_auth(args);
        int fd = connect_tcp(args.host, args.port);
        websocket_handshake(fd, args, token);
        StreamPlayer player;
        PlaybackState playback;
        if (args.play) {
            if (args.out_format == "opus" || args.out_format == "speex") {
                throw std::runtime_error("--play is unsupported for raw " + args.out_format +
                                         " packets; save them or pass each packet to the device decoder");
            }
            player.start(args.play_cmd.empty() ? default_play_command(args.out_format, args.out_rate) : args.play_cmd);
        }
        std::string conversation_id = "cpp-device-" + std::to_string(std::chrono::duration_cast<std::chrono::milliseconds>(
            std::chrono::system_clock::now().time_since_epoch()).count());
        if (!args.text.empty()) {
            send_text_turn(fd, conversation_id, args.text);
        } else {
            send_audio_stream(fd, args, conversation_id, input_audio);
        }
        bool interrupt_sent = false;
        std::string one_shot_turn_id;
        InterruptedOneShotCompletion interrupt_completion;
        std::string conversation_ended_turn_id;
        while (true) {
            std::string message = recv_ws_text(fd);
            if (message.empty()) break;
            const std::string event_type = extract_json_string(message, "event_type");
            const std::string event_turn_id = extract_json_string(message, "turn_id");
            if (playback.should_drop(message)) {
                std::cout << "dropped_interrupted_event=" << event_type
                          << " turn_id=" << event_turn_id << std::endl;
                continue;
            }
            if (event_type == "tts_audio_chunk") {
                playback.observe(message);
                if (one_shot_turn_id.empty()) {
                    one_shot_turn_id = event_turn_id;
                    interrupt_completion.begin(one_shot_turn_id);
                }
                std::vector<uint8_t> bytes = validate_tts_message(message, args, playback.sequence());
                const auto ready_chunks = playback.ordered().accept(message, bytes);
                if (!ready_chunks.empty() && args.play && !player.active()) {
                    player.start(args.play_cmd.empty()
                                     ? default_play_command(args.out_format, args.out_rate)
                                     : args.play_cmd);
                }
                for (const auto& ready : ready_chunks) {
                    append_file(args.output, encode_output_chunk(args.out_format, ready));
                    player.write(ready);
                }
                std::cout << "tts_audio_chunk format=" << extract_json_string(message, "format")
                          << " sample_rate=" << extract_json_value(message, "sample_rate")
                          << " seq=" << extract_json_value(message, "seq")
                          << " bytes=" << bytes.size()
                          << " last=" << extract_json_value(message, "is_last")
                          << std::endl;
                if (args.interrupt_after_first_chunk && !ready_chunks.empty() && !interrupt_sent) {
                    const ButtonInterruptResult interrupt_result = interrupt_tts_from_button(
                        playback,
                        [&player]() { player.stop_now(); },
                        [fd](const std::string& payload) { send_ws_text(fd, payload); });
                    if (interrupt_result == ButtonInterruptResult::SendFailed) {
                        throw std::runtime_error(
                            "TTS stopped locally, but tts_interrupt could not be sent");
                    }
                    interrupt_sent = interrupt_result == ButtonInterruptResult::Sent;
                }
            } else if (event_type == "tts_interrupted") {
                playback.observe(message);
                std::cout << message << std::endl;
                if (interrupt_completion.observe(event_type, event_turn_id)) break;
            } else if (event_type == "voice_done") {
                playback.ordered().finish();
                playback.observe(message);
                std::cout << message << std::endl;
                if (one_shot_turn_id.empty() || event_turn_id == one_shot_turn_id) break;
            } else if (event_type == "latency_metrics") {
                std::cout << message << std::endl;
                if (interrupt_sent && interrupt_completion.observe(event_type, event_turn_id)) break;
                if (!conversation_ended_turn_id.empty() &&
                    event_turn_id == conversation_ended_turn_id) {
                    break;
                }
            } else if (event_type == "conversation_ended") {
                playback.observe(message);
                std::cout << message << std::endl;
                if (event_turn_id.empty()) break;
                conversation_ended_turn_id = event_turn_id;
            } else {
                std::cout << message << std::endl;
            }
            if (event_type == "error") break;
        }
        player.close();
        close(fd);
        std::cout << "saved_tts=" << args.output << std::endl;
        return 0;
    } catch (const std::exception& error) {
        std::cerr << "error: " << error.what() << std::endl;
        return 1;
    }
}
