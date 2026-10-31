// Test-only in-memory device. No USB, network, firmware or real sensor access.
#pragma once
#include "IDevice.hpp"
namespace libobsensor {
class SyntheticDevice : public IDevice {
    std::shared_ptr<DeviceInfo> info_{new DeviceInfo};
    std::string yes_="true", sn_="synthetic-only";
public:
    SyntheticDevice() { info_->name_="Synthetic"; info_->deviceSn_=sn_; }
    void postInitialize() override {}
    void reset() override {}
    void reboot() override {}
    void deactivate(bool) override {}
    bool isPlaybackDevice() const override { return false; }
    bool hasAccessControl() const override { return false; }
    OBDeviceAccessMode getAccessMode() const override { return OB_DEVICE_EXCLUSIVE_ACCESS; }
    bool isAccessModeMatch(OBDeviceAccessMode) const override { return true; }
    std::shared_ptr<const DeviceInfo> getInfo() const override { return info_; }
    const std::string &getSn() const override { return sn_; }
    bool isDeactivated() const override { return false; }
    const std::string &getExtensionInfo(const std::string &) const override { return yes_; }
    bool isExtensionInfoExists(const std::string &) const override { return true; }
    uint64_t getDeviceErrorState() const override { return 0; }
    void fetchDeviceErrorState(uint64_t) override {}
    bool isComponentExists(DeviceComponentId) const override { return false; }
    bool isComponentCreated(DeviceComponentId) const override { return false; }
    DeviceComponentPtr<IDeviceComponent> getComponent(DeviceComponentId,bool) override { return {}; }
    DeviceComponentPtr<IPropertyServer> getPropertyServer() override { return {}; }
    bool isSensorExists(OBSensorType) const override { return false; }
    bool isSensorCreated(OBSensorType) const override { return false; }
    DeviceComponentPtr<ISensor> getSensor(OBSensorType) override { return {}; }
    std::vector<OBSensorType> getSensorTypeList() const override { return {OB_SENSOR_COLOR,OB_SENSOR_DEPTH}; }
    bool hasAnySensorStreamActivated() override { return false; }
    bool hasAnyVideoSensorStreamActivated() override { return false; }
    std::vector<std::shared_ptr<IFilter>> createRecommendedPostProcessingFilters(OBSensorType) override { return {}; }
    std::shared_ptr<IFilter> getSensorFrameFilter(const std::string &,OBSensorType,bool) override { return nullptr; }
    void updateFirmware(const std::vector<uint8_t>&,DeviceFwUpdateCallback,bool) override {}
    void setFirmwareUpdateState(bool) override {}
    bool isFirmwareUpdating() const override { return false; }
    void updateOptionalDepthPresets(const char[][OB_PATH_MAX],uint8_t,DeviceFwUpdateCallback) override {}
    void updateOptionalDepthPresets(const OBDataView*,uint8_t,DeviceFwUpdateCallback) override {}
    void activateDeviceAccessor() override {}
    void loadDefaultPostProcessingConfig() override {}
    int getFirmwareVersionInt() override { return 0; }
    void registerRebootCallback(DeviceRebootCallback) override {}
    bool hasWriteAccess() const override { return false; }
    void updateDepthPostProcessingFilterList() override {}
protected:
    void init() override {}
};
}
