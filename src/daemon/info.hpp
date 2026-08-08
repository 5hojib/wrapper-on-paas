#pragma once

#include <string>

namespace wrapper {

struct ServerInfo {
    std::string version = "1.0.0";
    bool apple_init_enabled = true;
};

}  // namespace wrapper
