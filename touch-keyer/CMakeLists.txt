cmake_minimum_required(VERSION 3.16)

# Firmware version. Bump here; it reaches the code as W1VE_FW_VERSION (defined on the main
# component) and shows in the boot log, the web page header and /api/status. A CACHE variable so
# the component CMakeLists in their own scope can read it.
set(W1VE_FW_VERSION "0.1.1" CACHE STRING "W1VE Touch Keyer firmware version")

include($ENV{IDF_PATH}/tools/cmake/project.cmake)
project(w1ve-touch-keyer VERSION ${W1VE_FW_VERSION})
