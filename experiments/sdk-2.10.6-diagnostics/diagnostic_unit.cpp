#include "utils/CaptureDiagnostics.hpp"
#include <cassert>
#include <cstring>
#include <thread>
#include <vector>
using namespace libobsensor;
int main(int argc,char **argv) {
    if(argc!=2)return 2;
    const std::string mode=argv[1];
    CaptureDiagnostics d;
    if(mode=="disabled") { assert(!d.enabled());assert(d.beforeLock()==0);for(int i=0;i<1000;++i)d.input(3);return 0; }
    assert(d.enabled());
    if(mode=="bounds") {
        for(int i=0;i<200;++i) { d.input(3);d.queue(3,3,3);d.policyDrop(3,i,1000+i,2000+i); }
        auto t=d.beforeLock();std::this_thread::sleep_for(std::chrono::milliseconds(2));
        { auto held=d.acquired(CaptureDiagnostics::PipelineCallback,t);std::this_thread::sleep_for(std::chrono::milliseconds(2)); }
    }
    else if(mode=="concurrent") {
        std::thread a([&] { for(int i=0;i<100000;++i) { d.input(2);d.event(CaptureDiagnostics::OutsideWindow,2,i,i,i+25); } });
        std::thread b([&] { for(int i=0;i<100000;++i) { d.input(3);d.event(CaptureDiagnostics::OutsideWindow,3,i,i,i+25); } });
        a.join();b.join();
    }
    else if(mode=="limit") {
        std::vector<std::unique_ptr<CaptureDiagnostics>> instances;
        for(int i=0;i<15;++i) { instances.emplace_back(new CaptureDiagnostics);assert(instances.back()->enabled()); }
        CaptureDiagnostics overflow;assert(!overflow.enabled());
    }
    else return 2;
}
