cmake_minimum_required(VERSION 3.16)
project(pulse-ring-panel LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_AUTOMOC ON)

find_package(Qt6 REQUIRED COMPONENTS Quick Gui)

qt_add_executable(pulse-ring-panel main.cpp)

qt_add_resources(pulse-ring-panel "qml"
    PREFIX "/"
    FILES Main.qml
)

target_link_libraries(pulse-ring-panel PRIVATE Qt6::Quick Qt6::Gui)
