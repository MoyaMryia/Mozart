// 搬运自 Mozart-Old/rvc-backend/src/rvc/index_search.cpp，改动：
//   - 引入 AArch64 NEON 向量化、32 帧质心分块与多线程并行检索。
#include "rvc/index_search.hpp"
#include <spdlog/spdlog.h>
#include <fstream>
#include <cmath>
#include <cstring>
#include <algorithm>
#include <limits>
#include <stdexcept>
#include <thread>
#if defined(__ARM_NEON) && defined(__aarch64__)
#include <arm_neon.h>
#endif

namespace rvc {

// ─────────────────────────────────────────────────────────────────────────────
// FAISS IndexIVFFlat（"IwFl"）序列化布局。
//
// 依据：faiss v1.7.2/v1.7.4/main 的 faiss/impl/index_write.cpp、index_read.cpp，
// 以及本地 faiss 1.15.1 生成文件 + HuggingFace 真实 RVC .index（added_IVF*_Flat_
// nprobe_1_*）的逐字节核对。四个版本布局一致：
//
//   u32  fourcc "IwFl"
//   i32  d
//   i64  ntotal
//   i64  dummy = 0x100000        （faiss 预留标记，恒为 1<<20，两处）
//   i64  dummy = 0x100000
//   u8   is_trained
//   i32  metric_type（0=IP，1=L2；>1 时还写 metric_arg，本实现拒绝）
//   i64  nlist
//   i64  nprobe
//   ── 内嵌量化器（IndexFlatL2 / IndexFlatIP）──
//   u32  fourcc "IxF2"（L2）或 "IxFI"（IP）
//   i32  d                      （== d）
//   i64  ntotal                 （== nlist）
//   i64  dummy ×2 = 0x100000
//   u8   is_trained
//   i32  metric_type
//   i64  质心元素数              （旧版写 float 个数 ntotal*d，新版部分写
//                                 字节数 ntotal*d*4，两者都接受）
//   f32  质心，nlist*d 个，行主序
//   ── direct map（RVC 索引恒为空）──
//   u8   map type（0=NoMap；1/2 时后面还跟变长数据，本实现拒绝）
//   i64  array 长度（0）
//   ── 倒排表 ──
//   u32  fourcc "ilar"
//   i64  nlist                  （== nlist）
//   i64  code_size              （== d*4）
//   u32  "full"（稠密）| "sprs"（稀疏）
//   full: i64 count（== nlist）+ count × i64 表大小
//   sprs: i64 对数 + 每对 (i64 表号, i64 大小)，未列出的表为空
//   每表：n*code_size 字节向量数据（float 原码），随后 n × i64 向量 id
// ─────────────────────────────────────────────────────────────────────────────

namespace {

constexpr uint32_t kHeaderDummy = 0x100000;   // faiss 预留标记 1<<20
constexpr uint64_t kMaxTotalVectors = 100000000ull;  // 1e8 × 3KB ≈ 300GB，纯防御

// 带边界检查的小端流式读取器；越界/超限一律抛异常，由 load() 统一记录。
class StreamReader {
public:
    explicit StreamReader(std::ifstream& f, uint64_t file_size)
        : f_(f), file_size_(file_size) {}

    void read(void* dst, size_t n) {
        if (pos_ + n > file_size_) {
            throw std::runtime_error("file truncated (need " + std::to_string(n)
                                     + " bytes at offset " + std::to_string(pos_)
                                     + ", file is " + std::to_string(file_size_) + ")");
        }
        f_.read(reinterpret_cast<char*>(dst), static_cast<std::streamsize>(n));
        if (!f_) throw std::runtime_error("read failed at offset " + std::to_string(pos_));
        pos_ += n;
    }

    void skip(size_t n) {
        char scratch[256];
        while (n > 0) {
            const size_t chunk = std::min(n, sizeof(scratch));
            read(scratch, chunk);
            n -= chunk;
        }
    }

    uint8_t u8() { uint8_t v; read(&v, 1); return v; }
    int32_t i32() { int32_t v; read(&v, 4); return v; }
    int64_t i64() { int64_t v; read(&v, 8); return v; }

    std::string fourcc() {
        char c[4];
        read(c, 4);
        return std::string(c, 4);
    }

    void expect_fourcc(const char* expected, const char* what) {
        const auto got = fourcc();
        if (got != expected) {
            throw std::runtime_error(std::string(what) + ": expected fourcc '"
                                     + expected + "', got '" + got + "'");
        }
    }

    float f32() { float v; read(&v, 4); return v; }

    uint64_t pos() const { return pos_; }
    uint64_t remaining() const { return file_size_ - pos_; }

private:
    std::ifstream& f_;
    uint64_t file_size_;
    uint64_t pos_ = 0;
};

} // namespace

bool IndexSearch::load(const std::filesystem::path& index_path, uint32_t feature_dim) {
    nlist_ = 0;
    ntotal_ = 0;
    skipped_frames_ = 0;
    centroids_.clear();
    codes_.clear();
    list_offsets_.clear();
    feature_dim_ = feature_dim;

    if (!std::filesystem::exists(index_path)) {
        spdlog::warn("Index file not found: {}", index_path.string());
        return false;
    }

    try {
        if (!parse_ivf_index(index_path)) {
            return false;
        }
        spdlog::info("Index loaded: {} (nlist={}, ntotal={}, dim={})",
            index_path.filename().string(), nlist_, ntotal_, feature_dim_);
        return true;
    } catch (const std::exception& e) {
        nlist_ = 0;
        ntotal_ = 0;
        centroids_.clear();
        codes_.clear();
        list_offsets_.clear();
        spdlog::error("Failed to load index {}: {}", index_path.string(), e.what());
        return false;
    }
}

bool IndexSearch::parse_ivf_index(const std::filesystem::path& path) {
    std::ifstream f(path, std::ios::binary);
    if (!f) return false;
    const auto file_size = static_cast<uint64_t>(std::filesystem::file_size(path));

    StreamReader r(f, file_size);

    r.expect_fourcc("IwFl", "not an FAISS IndexIVFFlat index");

    const int32_t d = r.i32();
    if (d <= 0 || d > 65536) {
        spdlog::error("FAISS IVF index has invalid header: d={}", d);
        return false;
    }
    feature_dim_ = static_cast<uint32_t>(d);

    const int64_t ntotal = r.i64();
    if (ntotal < 0 || static_cast<uint64_t>(ntotal) > kMaxTotalVectors) {
        throw std::runtime_error("invalid ntotal " + std::to_string(ntotal));
    }
    ntotal_ = static_cast<uint64_t>(ntotal);

    if (r.i64() != kHeaderDummy || r.i64() != kHeaderDummy) {
        throw std::runtime_error(
            "missing FAISS header marker (0x100000); only faiss >= 1.7.0 "
            "IndexIVFFlat files are supported");
    }

    if (r.u8() == 0) {
        throw std::runtime_error("index is not trained");
    }
    const int32_t metric = r.i32();
    if (metric != 1) {  // faiss METRIC_L2
        throw std::runtime_error("unsupported metric_type " + std::to_string(metric)
                                 + " (only L2, as used by RVC, is supported)");
    }

    const int64_t nlist = r.i64();
    if (nlist <= 0 || nlist > 1000000) {
        throw std::runtime_error("invalid nlist " + std::to_string(nlist)
                                 + " for ntotal " + std::to_string(ntotal));
    }
    nlist_ = static_cast<uint32_t>(nlist);
    r.i64();  // nprobe：检索端恒按 nprobe=1（RVC 约定）逐表精确搜索

    // ── 内嵌量化器 ──
    const auto qfourcc = r.fourcc();
    if (qfourcc != "IxF2" && qfourcc != "IxFI") {
        throw std::runtime_error("unsupported quantizer fourcc '" + qfourcc
                                 + "' (expected flat L2/IP)");
    }
    const int32_t qd = r.i32();
    const int64_t qntotal = r.i64();
    if (qd != d || qntotal != nlist) {
        throw std::runtime_error("quantizer header mismatch: d=" + std::to_string(qd)
                                 + " ntotal=" + std::to_string(qntotal));
    }
    if (r.i64() != kHeaderDummy || r.i64() != kHeaderDummy) {
        throw std::runtime_error("missing quantizer header marker");
    }
    if (r.u8() == 0) {
        throw std::runtime_error("quantizer is not trained");
    }
    const int32_t qmetric = r.i32();
    if (qmetric != metric) {
        throw std::runtime_error("quantizer metric " + std::to_string(qmetric)
                                 + " disagrees with index metric");
    }

    const int64_t count = r.i64();
    const int64_t floats_expected = nlist * d;
    if (count != floats_expected && count != floats_expected * 4) {
        throw std::runtime_error("unexpected quantizer codes count " + std::to_string(count));
    }

    if (static_cast<uint64_t>(floats_expected) > r.remaining() / sizeof(float)) {
        throw std::runtime_error("file too small for quantizer centroids");
    }
    centroids_.resize(static_cast<size_t>(floats_expected));
    for (auto& c : centroids_) c = r.f32();

    // ── direct map ──
    const uint8_t map_type = r.u8();
    if (map_type != 0) {
        throw std::runtime_error("unsupported direct map type " + std::to_string(map_type));
    }
    if (r.i64() != 0) {
        throw std::runtime_error("direct map array must be empty");
    }

    // ── 倒排表 ──
    r.expect_fourcc("ilar", "unsupported inverted lists storage");

    const int64_t il_nlist = r.i64();
    const int64_t code_size = r.i64();
    if (il_nlist != nlist || code_size != static_cast<int64_t>(d) * 4) {
        throw std::runtime_error("inverted lists header mismatch: nlist="
                                 + std::to_string(il_nlist) + " code_size="
                                 + std::to_string(code_size));
    }

    std::vector<uint64_t> sizes(static_cast<size_t>(nlist), 0);
    const auto list_type = r.fourcc();
    if (list_type == "full") {
        if (r.i64() != nlist) {
            throw std::runtime_error("'full' list count mismatch");
        }
        for (auto& s : sizes) {
            const int64_t v = r.i64();
            if (v < 0) throw std::runtime_error("negative list size");
            s = static_cast<uint64_t>(v);
        }
    } else if (list_type == "sprs") {
        const int64_t pairs = r.i64();
        if (pairs < 0 || pairs > nlist || static_cast<uint64_t>(pairs) > r.remaining() / 16) {
            throw std::runtime_error("invalid 'sprs' pair count");
        }
        std::vector<bool> seen(static_cast<size_t>(nlist), false);
        for (int64_t i = 0; i < pairs; ++i) {
            const int64_t idx = r.i64();
            const int64_t v = r.i64();
            if (idx < 0 || idx >= nlist || v < 0) {
                throw std::runtime_error("invalid 'sprs' entry");
            }
            if (seen[static_cast<size_t>(idx)]) {
                throw std::runtime_error("duplicate 'sprs' list id");
            }
            seen[static_cast<size_t>(idx)] = true;
            sizes[static_cast<size_t>(idx)] = static_cast<uint64_t>(v);
        }
    } else {
        throw std::runtime_error("unsupported list storage '" + list_type
                                 + "' (expected 'full' or 'sprs')");
    }

    uint64_t total = 0;
    for (const auto s : sizes) {
        total += s;
        if (total > kMaxTotalVectors) {
            throw std::runtime_error("list sizes exceed sanity bound");
        }
    }
    if (total != ntotal_) {
        throw std::runtime_error("list sizes sum " + std::to_string(total)
                                 + " != ntotal " + std::to_string(ntotal));
    }

    // 每表：n × code_size 字节向量数据 + n × 8 字节 id。
    // 先按剩余字节数校验总预算，再逐表读取，避免坏文件触发超大分配。
    const uint64_t bytes_per_vector = static_cast<uint64_t>(d) * 4 + 8;
    if (total > r.remaining() / bytes_per_vector) {
        throw std::runtime_error("file too small for " + std::to_string(total)
                                 + " vectors (truncated?)");
    }

    codes_.resize(static_cast<size_t>(ntotal_) * static_cast<size_t>(d));
    list_offsets_.assign(static_cast<size_t>(nlist) + 1, 0);
    uint64_t written = 0;
    std::vector<int64_t> id_scratch;
    for (uint32_t i = 0; i < nlist_; ++i) {
        list_offsets_[i] = written;
        const uint64_t n = sizes[i];
        if (n == 0) continue;
        float* dst = codes_.data() + written * static_cast<uint64_t>(d);
        r.read(dst, static_cast<size_t>(n) * static_cast<size_t>(d) * 4);
        id_scratch.resize(static_cast<size_t>(n));
        r.read(id_scratch.data(), static_cast<size_t>(n) * 8);
        written += n;
    }
    list_offsets_[nlist_] = written;
    if (r.remaining() != 0) {
        throw std::runtime_error(std::to_string(r.remaining())
                                 + " unexpected trailing bytes");
    }

    spdlog::info("Parsed {} centroid(s), {} vector(s)", nlist_, ntotal_);
    return true;
}

namespace {

inline float l2_distance_sq(const float* a, const float* b, size_t dim) noexcept {
#if defined(__ARM_NEON) && defined(__aarch64__)
    float32x4_t acc0 = vdupq_n_f32(0.0f);
    float32x4_t acc1 = vdupq_n_f32(0.0f);
    float32x4_t acc2 = vdupq_n_f32(0.0f);
    float32x4_t acc3 = vdupq_n_f32(0.0f);
    size_t j = 0;
    for (; j + 16 <= dim; j += 16) {
        const float32x4_t d0 = vsubq_f32(vld1q_f32(a + j), vld1q_f32(b + j));
        const float32x4_t d1 = vsubq_f32(vld1q_f32(a + j + 4), vld1q_f32(b + j + 4));
        const float32x4_t d2 = vsubq_f32(vld1q_f32(a + j + 8), vld1q_f32(b + j + 8));
        const float32x4_t d3 = vsubq_f32(vld1q_f32(a + j + 12), vld1q_f32(b + j + 12));
        acc0 = vfmaq_f32(acc0, d0, d0);
        acc1 = vfmaq_f32(acc1, d1, d1);
        acc2 = vfmaq_f32(acc2, d2, d2);
        acc3 = vfmaq_f32(acc3, d3, d3);
    }
    acc0 = vaddq_f32(vaddq_f32(acc0, acc1), vaddq_f32(acc2, acc3));
    for (; j + 4 <= dim; j += 4) {
        const float32x4_t d0 = vsubq_f32(vld1q_f32(a + j), vld1q_f32(b + j));
        acc0 = vfmaq_f32(acc0, d0, d0);
    }
    float dist = vaddvq_f32(acc0);
    for (; j < dim; ++j) {
        const float dv = a[j] - b[j];
        dist += dv * dv;
    }
    return dist;
#else
    float acc0 = 0.0f;
    float acc1 = 0.0f;
    float acc2 = 0.0f;
    float acc3 = 0.0f;
    size_t j = 0;
    for (; j + 4 <= dim; j += 4) {
        const float d0 = a[j] - b[j];
        const float d1 = a[j + 1] - b[j + 1];
        const float d2 = a[j + 2] - b[j + 2];
        const float d3 = a[j + 3] - b[j + 3];
        acc0 += d0 * d0;
        acc1 += d1 * d1;
        acc2 += d2 * d2;
        acc3 += d3 * d3;
    }
    float dist = (acc0 + acc1) + (acc2 + acc3);
    for (; j < dim; ++j) {
        const float dv = a[j] - b[j];
        dist += dv * dv;
    }
    return dist;
#endif
}

struct CentroidEntry {
    float dist;
    uint32_t id;
};

inline void insert_centroid(
    std::vector<CentroidEntry>& best, uint32_t nprobe, float dist, uint32_t id
) {
    if (best.size() == nprobe && dist >= best.back().dist) return;
    const auto pos = std::lower_bound(best.begin(), best.end(), dist,
        [](const CentroidEntry& e, float value) { return e.dist < value; });
    best.insert(pos, CentroidEntry{dist, id});
    if (best.size() > nprobe) best.pop_back();
}

struct ThreadJoiner {
    std::vector<std::thread>& threads;
    ~ThreadJoiner() {
        for (auto& th : threads) {
            if (th.joinable()) th.join();
        }
    }
};

template <typename Fn>
void run_parallel(size_t workers, Fn&& fn) {
    if (workers <= 1) {
        fn(0);
        return;
    }
    std::vector<std::thread> threads;
    threads.reserve(workers - 1);
    ThreadJoiner guard{threads};
    for (size_t w = 1; w < workers; ++w) threads.emplace_back(fn, w);
    fn(0);
}

} // namespace

void IndexSearch::find_nearest_centroids(const float* vec, std::vector<uint32_t>& ids) const {
    const size_t dim = feature_dim_;
    ids.clear();
    if (nlist_ == 0) return;
    if (nprobe_ == 1) {
        float best_dist = std::numeric_limits<float>::infinity();
        uint32_t best_id = 0;
        for (uint32_t i = 0; i < nlist_; ++i) {
            const float* c = centroids_.data() + static_cast<size_t>(i) * dim;
            const float dist = l2_distance_sq(vec, c, dim);
            if (dist < best_dist) {
                best_dist = dist;
                best_id = i;
            }
        }
        ids.push_back(best_id);
        return;
    }
    std::vector<CentroidEntry> best;
    best.reserve(nprobe_);
    for (uint32_t i = 0; i < nlist_; ++i) {
        const float* c = centroids_.data() + static_cast<size_t>(i) * dim;
        insert_centroid(best, nprobe_, l2_distance_sq(vec, c, dim), i);
    }
    ids.reserve(best.size());
    for (const auto& e : best) ids.push_back(e.id);
}

std::vector<float> IndexSearch::search(
    const std::vector<float>& features,
    uint32_t feat_dim, float index_rate
) {
    if (!loaded() || index_rate <= 0.0f) {
        return features;
    }

    if (feat_dim != feature_dim_) {
        spdlog::warn("Feature dim {} != index dim {}, skip index", feat_dim, feature_dim_);
        return features;
    }

    const size_t n_frames = features.size() / feat_dim;
    if (n_frames == 0) {
        return features;
    }
    std::vector<float> result = features;
    const size_t dim = feature_dim_;
    const uint32_t nprobe = std::min(nprobe_, nlist_);

    const unsigned hw = std::thread::hardware_concurrency();
    const size_t max_workers = hw > 0 ? std::min<size_t>(4, hw) : 1;
    const bool parallel = max_workers > 1 && nlist_ >= 256 && n_frames >= 2;
    const size_t workers = parallel ? max_workers : 1;

    // 1. 质心检索：按质心分片、查询帧按 32 帧分块，使每个质心只从主存读一次，
    //    避免 n_frames 次重复扫描 10+ MB 质心表。
    constexpr size_t kQueryTile = 32;
    std::vector<uint32_t> frame_centroids(n_frames * nprobe, 0);
    if (nprobe == 1) {
        std::vector<std::vector<float>> part_dist(workers, std::vector<float>(n_frames, std::numeric_limits<float>::infinity()));
        std::vector<std::vector<uint32_t>> part_id(workers, std::vector<uint32_t>(n_frames, 0));
        auto scan_centroids = [&](size_t w) {
            const uint32_t c_begin = static_cast<uint32_t>((static_cast<uint64_t>(nlist_) * w) / workers);
            const uint32_t c_end = static_cast<uint32_t>((static_cast<uint64_t>(nlist_) * (w + 1)) / workers);
            auto& best_d = part_dist[w];
            auto& best_i = part_id[w];
            for (size_t tile = 0; tile < n_frames; tile += kQueryTile) {
                const size_t t_end = std::min(tile + kQueryTile, n_frames);
                for (uint32_t ci = c_begin; ci < c_end; ++ci) {
                    const float* c = centroids_.data() + static_cast<size_t>(ci) * dim;
                    for (size_t t = tile; t < t_end; ++t) {
                        const float* q = features.data() + t * dim;
                        const float dist = l2_distance_sq(q, c, dim);
                        if (dist < best_d[t]) {
                            best_d[t] = dist;
                            best_i[t] = ci;
                        }
                    }
                }
            }
        };
        run_parallel(workers, scan_centroids);
        for (size_t t = 0; t < n_frames; ++t) {
            float best_d = part_dist[0][t];
            uint32_t best_i = part_id[0][t];
            for (size_t w = 1; w < workers; ++w) {
                if (part_dist[w][t] < best_d) {
                    best_d = part_dist[w][t];
                    best_i = part_id[w][t];
                }
            }
            frame_centroids[t] = best_i;
        }
    } else {
        std::vector<std::vector<std::vector<CentroidEntry>>> part_best(
            workers, std::vector<std::vector<CentroidEntry>>(n_frames));
        auto scan_centroids = [&](size_t w) {
            const uint32_t c_begin = static_cast<uint32_t>((static_cast<uint64_t>(nlist_) * w) / workers);
            const uint32_t c_end = static_cast<uint32_t>((static_cast<uint64_t>(nlist_) * (w + 1)) / workers);
            auto& local = part_best[w];
            for (size_t t = 0; t < n_frames; ++t) local[t].reserve(nprobe);
            for (size_t tile = 0; tile < n_frames; tile += kQueryTile) {
                const size_t t_end = std::min(tile + kQueryTile, n_frames);
                for (uint32_t ci = c_begin; ci < c_end; ++ci) {
                    const float* c = centroids_.data() + static_cast<size_t>(ci) * dim;
                    for (size_t t = tile; t < t_end; ++t) {
                        const float* q = features.data() + t * dim;
                        insert_centroid(local[t], nprobe, l2_distance_sq(q, c, dim), ci);
                    }
                }
            }
        };
        run_parallel(workers, scan_centroids);
        std::vector<CentroidEntry> merged;
        merged.reserve(nprobe);
        for (size_t t = 0; t < n_frames; ++t) {
            merged.clear();
            for (size_t w = 0; w < workers; ++w) {
                for (const auto& e : part_best[w][t]) {
                    insert_centroid(merged, nprobe, e.dist, e.id);
                }
            }
            for (size_t k = 0; k < merged.size(); ++k) {
                frame_centroids[t * nprobe + k] = merged[k].id;
            }
        }
    }

    // 2. 倒排表精确 k=8 检索与加权混合：帧间完全独立，按帧切片并行。
    struct Candidate { float dist; const float* vec; };
    const size_t blend_workers = std::min(workers, n_frames);
    std::vector<uint64_t> local_skipped(blend_workers, 0);
    auto search_frames = [&](size_t w) {
        const size_t t_begin = (n_frames * w) / blend_workers;
        const size_t t_end = (n_frames * (w + 1)) / blend_workers;
        uint64_t skipped = 0;
        for (size_t t = t_begin; t < t_end; ++t) {
            const float* feat_vec = features.data() + t * dim;
            Candidate best[kNeighbours];
            size_t count = 0;
            for (uint32_t p = 0; p < nprobe; ++p) {
                const uint32_t cid = frame_centroids[t * nprobe + p];
                const uint64_t begin = list_offsets_[cid];
                const uint64_t end = list_offsets_[cid + 1];
                for (uint64_t k = begin; k < end; ++k) {
                    const float* cand = codes_.data() + k * dim;
                    const float dist = l2_distance_sq(feat_vec, cand, dim);
                    if (count == kNeighbours && dist >= best[count - 1].dist) continue;
                    size_t pos = count < kNeighbours ? count : kNeighbours - 1;
                    while (pos > 0 && best[pos - 1].dist > dist) {
                        if (pos < kNeighbours) best[pos] = best[pos - 1];
                        --pos;
                    }
                    best[pos] = Candidate{dist, cand};
                    if (count < kNeighbours) ++count;
                }
            }

            // Golden 路径用 k=8 且要求全部命中；候选不足 8 个时整帧不混合。
            if (count < kNeighbours) {
                ++skipped;
                continue;
            }

            // Golden: weight = (1 / distance)^2, then normalise.
            float weight[kNeighbours];
            float total = 0.0f;
            for (size_t i = 0; i < kNeighbours; ++i) {
                const float inv = 1.0f / (best[i].dist + 1e-12f);
                weight[i] = inv * inv;
                total += weight[i];
            }
            const float norm = 1.0f / total;
            for (size_t i = 0; i < kNeighbours; ++i) weight[i] *= norm;
            float* out_vec = result.data() + t * dim;
            const float keep = 1.0f - index_rate;
            for (size_t j = 0; j < dim; ++j) {
                float blended = 0.0f;
                for (size_t i = 0; i < kNeighbours; ++i) {
                    blended += weight[i] * best[i].vec[j];
                }
                out_vec[j] = feat_vec[j] * keep + blended * index_rate;
            }
        }
        local_skipped[w] = skipped;
    };
    run_parallel(blend_workers, search_frames);
    for (uint64_t s : local_skipped) skipped_frames_ += s;

    return result;
}

} // namespace rvc
