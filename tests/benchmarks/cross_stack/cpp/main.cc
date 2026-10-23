#include <drogon/drogon.h>
#include <drogon/orm/DbClient.h>
#include <json/json.h>
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstdio>
#include <deque>
#include <fstream>
#include <functional>
#include <iostream>
#include <mutex>
#include <thread>
using namespace drogon;
namespace {
std::atomic<int64_t> active{0}, waiting{0}, jobs{0}, dbPending{0};
int env(const char *name, int fallback) { auto p = std::getenv(name); return p ? std::stoi(p) : fallback; }
int64_t checksum(int iterations, int seed) { int64_t value = seed; for (int64_t i = 0; i < iterations; ++i) value = (value * 33 + i + seed) % 100000000; return value; }
std::string encode(const Json::Value &value) { Json::StreamWriterBuilder writer; writer["indentation"] = ""; return Json::writeString(writer, value); }
bool parse(std::string_view text, Json::Value &value) { Json::CharReaderBuilder builder; builder["collectComments"] = false; builder["failIfExtra"] = true; builder["allowComments"] = false; builder["allowTrailingCommas"] = false; std::unique_ptr<Json::CharReader> reader(builder.newCharReader()); std::string errors; return reader->parse(text.data(), text.data() + text.size(), &value, &errors); }
using Callback = std::function<void(const HttpResponsePtr &)>;
void send(const Callback &cb, HttpStatusCode code, std::string body, bool json = false) { auto r = HttpResponse::newHttpResponse(); r->setStatusCode(code); r->setContentTypeString(json ? "application/json; charset=utf-8" : "text/plain; charset=utf-8"); r->setBody(std::move(body)); cb(r); --active; }
class CpuPool {
 std::mutex mutex; std::condition_variable wake; std::deque<std::function<void()>> queue; std::vector<std::thread> threads; bool stop = false;
public:
 explicit CpuPool(int n) { for (int i = 0; i < n; i++) threads.emplace_back([this] { for (;;) { std::function<void()> work; { std::unique_lock lock(mutex); wake.wait(lock, [this] { return stop || !queue.empty(); }); if (stop && queue.empty()) return; work = std::move(queue.front()); queue.pop_front(); } work(); } }); }
 bool submit(std::function<void()> work) { std::lock_guard lock(mutex); if (stop || queue.size() >= 128) return false; queue.push_back(std::move(work)); wake.notify_one(); return true; }
 ~CpuPool() { { std::lock_guard lock(mutex); stop = true; } wake.notify_all(); for (auto &t : threads) t.join(); }
};
}
int main(int argc, char **argv) {
 if (argc > 1 && std::string(argv[1]) == "--batch") { std::ifstream f(argc > 2 ? argv[2] : "batch.json"); Json::Value input; f >> input; int iterations = input["iterations"].asInt(), seed = input["seed"].asInt(), repeat = input["repeat"].asInt(); if (iterations < 1000 || iterations > 1000000 || seed < 1 || seed > 1024 || repeat < 0 || repeat > 32768) return 2; std::vector<int64_t> values(repeat); auto start = std::chrono::steady_clock::now(); for (auto &v : values) v = checksum(iterations, seed); double ms = std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - start).count(); Json::Value result; result["checksums"] = Json::Value(Json::arrayValue); for (auto v : values) result["checksums"].append(Json::Int64(v)); result["compute_ms"] = ms; std::cout << encode(result) << '\n'; return 0; }
 int workers = env("BENCH_WORKERS", 1); CpuPool cpu(workers);
 orm::DbClientPtr db; if (auto url = std::getenv("BENCH_DB_URL")) { db = orm::DbClient::newPgClient(url, env("BENCH_DB_POOL", 16)); db->setTimeout(45); }
 auto handler = [&, db](const HttpRequestPtr &req, Callback &&cb) {
  ++active; auto rid = req->getParameter("rid");
  try {
   if (req->path() == "/metrics") { Json::Value m; m["active"] = Json::Int64(active.load() - 1); m["waiting_timer"] = Json::Int64(waiting.load()); m["waiting_tcp"] = 0; m["actual_cpu_jobs"] = Json::Int64(jobs.load()); m["database_pending"] = Json::Int64(dbPending.load()); send(cb, k200OK, encode(m), true); }
   else if (req->path() == "/ok") send(cb, k200OK, "ok:" + rid);
   else if (req->path() == "/json") {
    Json::Value input; if (req->method() != Post) { send(cb, k405MethodNotAllowed, "method_not_allowed"); return; }
    if (req->body().size() > 65536 || !parse(req->body(), input) || !input.isObject() || !input.isMember("data") || !input["counter"].isInt64() || input["counter"].asInt64() < -9007199254740990LL || input["counter"].asInt64() > 9007199254740990LL) { send(cb, k400BadRequest, "invalid_json_envelope"); return; }
    Json::Value output; output["id"] = rid; output["counter"] = Json::Int64(input["counter"].asInt64() + 1); output["data"] = std::move(input["data"]); send(cb, k200OK, encode(output), true);
   }
   else if (req->path() == "/timer") { ++waiting; trantor::EventLoop::getEventLoopOfCurrentThread()->runAfter(.025, [cb = std::move(cb), rid] { --waiting; send(cb, k200OK, "timer:" + rid); }); }
   else if (req->path() == "/work") {
    int iterations = req->getParameter("work").size() * 1000, seed = req->getParameter("seed").size(); if (iterations < 1000 || iterations > 1000000 || seed < 1 || seed > 1024) { send(cb, k400BadRequest, "invalid_cpu_input"); return; }
    ++jobs; auto loop = trantor::EventLoop::getEventLoopOfCurrentThread(); auto reply = cb;
    if (!cpu.submit([reply, loop, rid, iterations, seed] { auto value = checksum(iterations, seed); --jobs; loop->queueInLoop([reply, rid, value] { char number[32]; std::snprintf(number, sizeof number, "%08lld", (long long)value); send(reply, k200OK, rid + ":" + number); }); })) { --jobs; send(cb, k503ServiceUnavailable, "cpu_queue_full"); }
   }
   else if (req->path() == "/db") {
    if (!db) { send(cb, k503ServiceUnavailable, "database_not_configured"); return; } int key; try { key = std::stoi(req->getParameter("key")); } catch (...) { send(cb, k400BadRequest, "invalid_database_key"); return; } if (key < 1 || key > 1024) { send(cb, k400BadRequest, "invalid_database_key"); return; }
    ++dbPending; db->execSqlAsync("SELECT id,label,score FROM cross_stack_items WHERE id=$1", [cb, rid](const orm::Result &rows) { --dbPending; if (rows.size() != 1) { send(cb, k500InternalServerError, "database_row_error"); return; } Json::Value out; out["id"] = rows[0][0].as<int>(); out["label"] = rows[0][1].as<std::string>(); out["score"] = rows[0][2].as<int>(); out["rid"] = rid; send(cb, k200OK, encode(out), true); }, [cb](const orm::DrogonDbException &) { --dbPending; send(cb, k500InternalServerError, "database_query_error"); }, key);
   } else send(cb, k404NotFound, "not_found");
  } catch (...) { send(cb, k500InternalServerError, "handler_error"); }
 };
 auto addr = std::string(std::getenv("BENCH_ADDR") ? std::getenv("BENCH_ADDR") : "127.0.0.1:19090"); auto colon = addr.rfind(':');
 app().setLogLevel(trantor::Logger::kWarn).setThreadNum(workers).setIdleConnectionTimeout(5).setClientMaxBodySize(65536).setClientMaxMemoryBodySize(65536).enableGzip(false).enableBrotli(false).addListener(addr.substr(0, colon), std::stoi(addr.substr(colon + 1)));
 for (const auto &path : {"/ok", "/json", "/work", "/timer", "/db", "/metrics"}) app().registerHandler(path, handler, {Get, Post});
 app().run(); return 0;
}
