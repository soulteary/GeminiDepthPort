#include "Pipeline.hpp"
#include "logger/Logger.hpp"
#include "fake_device.hpp"
#include "frame/FrameFactory.hpp"
#include <algorithm>
#include <chrono>
#include <iostream>
#include <fstream>
#include <numeric>
#include <sys/resource.h>
using namespace libobsensor;
static uint64_t ns() { return std::chrono::duration_cast<std::chrono::nanoseconds>(std::chrono::steady_clock::now().time_since_epoch()).count(); }
static double cpu() { struct rusage r; getrusage(RUSAGE_SELF,&r); return r.ru_utime.tv_sec+r.ru_utime.tv_usec/1e6+r.ru_stime.tv_sec+r.ru_stime.tv_usec/1e6; }
struct Input {
    std::shared_ptr<Config> cfg{new Config};
    std::shared_ptr<const StreamProfile> color,depth;
    Input(OBFrameAggregateOutputMode mode) {
        cfg->enableVideoStream(OB_STREAM_COLOR,2,2,30,OB_FORMAT_YUYV);
        cfg->enableVideoStream(OB_STREAM_DEPTH,2,2,30,OB_FORMAT_Y16);
        cfg->setFrameAggregateOutputMode(mode);
        color=cfg->getEnabledStreamProfile(OB_STREAM_COLOR);depth=cfg->getEnabledStreamProfile(OB_STREAM_DEPTH);
    }
    std::shared_ptr<Frame> frame(bool isDepth,uint64_t index,uint64_t stamp) {
        auto f=FrameFactory::createFrameFromStreamProfile(isDepth?depth:color);
        f->setNumber(index);f->setTimeStampUsec(stamp);f->setSystemTimeStampUsec(1700000000000000+stamp);
        std::memset(f->getDataMutable(),static_cast<int>(index%256),f->getDataSize());
        return f;
    }
};
int main(int argc,char **argv) {
    Logger::setLogSeverity(OB_LOG_SEVERITY_OFF);
    if(argc!=4) { std::cerr<<"usage: probe scenario iterations pace_us\n";return 2; }
    const std::string mode=argv[1];const uint64_t loops=std::stoull(argv[2]),pace=std::stoull(argv[3]);
    if(!loops||loops>200000||pace>1000000) return 2;
    const bool any=mode=="partial", overflow=mode=="overflow", skew=mode=="skew", starved=mode=="starved";
    if(mode!="paired"&&!any&&!overflow&&!skew&&!starved&&mode!="pipeline-lock"&&mode!="aggregator-lock") return 2;
    uint64_t outputSets=0,outputFrames=0,digest=1469598103934665603ULL;
    std::vector<uint64_t> delays;delays.reserve(loops*2);
    const double cpuStart=cpu();const uint64_t started=ns();
    {
        Pipeline pipeline(std::make_shared<SyntheticDevice>());
        Input input(any ? OB_FRAME_AGGREGATE_OUTPUT_ANY_SITUATION : OB_FRAME_AGGREGATE_OUTPUT_ALL_TYPE_FRAME_REQUIRE);
        pipeline.config_=input.cfg;
        pipeline.frameAggregator_->updateConfig(input.cfg,true);
        pipeline.streamState_=STREAM_STATE_STARTING;
        pipeline.maxFrameQueueSize_=overflow?2:10;pipeline.outputFrameQueue_->resize(pipeline.maxFrameQueueSize_);
        auto consume=[&]() {
            while(auto f=pipeline.waitForFrame(0)) {
                ++outputSets;
                auto set=f->as<FrameSet>();
                for(uint32_t i=0;i<set->getCount();++i) {
                    auto frame=set->getFrame(i);++outputFrames;
                    for(uint64_t n : {static_cast<uint64_t>(frame->getType()),frame->getNumber(),frame->getTimeStampUsec(),frame->getSystemTimeStampUsec(),static_cast<uint64_t>(frame->getDataSize())}) { digest^=n;digest*=1099511628211ULL; }
                    const uint8_t *data=frame->getData();for(size_t n=0;n<frame->getDataSize();++n) { digest^=data[n];digest*=1099511628211ULL; }
                }
            }
        };
        for(uint64_t n=1;n<=loops;++n) {
            const uint64_t stamp=1000000+n*33333;
            auto color=input.frame(false,n,stamp);
            auto depth=input.frame(true,n,stamp+(skew&&n%60<4?25000:0));
            std::thread blocker;
            std::atomic<bool> held(false);
            if(n==1 && (mode=="pipeline-lock"||mode=="aggregator-lock")) {
                blocker=std::thread([&] {
                    if(mode=="pipeline-lock") { std::lock_guard<std::mutex> lock(pipeline.streamMutex_);held.store(true);std::this_thread::sleep_for(std::chrono::milliseconds(30)); }
                    else { std::lock_guard<std::recursive_mutex> lock(pipeline.frameAggregator_->srcFrameQueueMutex_);held.store(true);std::this_thread::sleep_for(std::chrono::milliseconds(30)); }
                });
                while(!held.load()) std::this_thread::yield();
            }
            const uint64_t t=ns();pipeline.onFrameCallback(color);delays.push_back(ns()-t);
            if(blocker.joinable()) blocker.join();
            if(!starved&&(!any||n%7)) { const uint64_t d=ns();pipeline.onFrameCallback(depth);delays.push_back(ns()-d); }
            if(!overflow||n%13==0) consume();
            if(pace) std::this_thread::sleep_until(std::chrono::steady_clock::time_point(std::chrono::nanoseconds(started+n*pace*1000)));
        }
        consume();
        pipeline.streamState_=STREAM_STATE_STOPPED; // No synthetic hardware stop calls.
    }
    const double wall=(ns()-started)/1e9,used=cpu()-cpuStart;
    std::sort(delays.begin(),delays.end());
    struct rusage r;getrusage(RUSAGE_SELF,&r);
    uint64_t peak=0;
    std::ifstream status("/proc/self/status");std::string line;
    while(std::getline(status,line)) if(line.compare(0,6,"VmHWM:")==0) { std::istringstream field(line);std::string key;field>>key>>peak; }
    if(!peak) { std::cerr<<"VmHWM unavailable\n";return 3; }
    std::cout<<"{\"scenario\":\""<<mode<<"\",\"iterations\":"<<loops<<",\"pace_us\":"<<pace<<",\"output_sets\":"<<outputSets<<",\"output_frames\":"<<outputFrames<<",\"behavior_digest\":"<<digest<<",\"cpu_seconds\":"<<used<<",\"wall_seconds\":"<<wall<<",\"cpu_cores\":"<<used/wall<<",\"peak_rss_kib\":"<<peak<<",\"rusage_maxrss_kib\":"<<r.ru_maxrss<<",\"callback_p99_ns\":"<<delays[(delays.size()*99-1)/100]<<",\"hardware_verified\":false}\n";
}
