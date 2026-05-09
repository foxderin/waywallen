find_package(Vulkan REQUIRED)
find_package(Threads REQUIRED)

add_executable(waywallen-video-renderer
    src/main.cpp
)
set_target_properties(waywallen-video-renderer PROPERTIES 
    INSTALL_RPATH "${WAYWALLEN_BIN_RPATH}"
)
target_compile_features(waywallen-video-renderer PRIVATE cxx_std_20)
set_target_properties(waywallen-video-renderer PROPERTIES CXX_SCAN_FOR_MODULES ON)
target_compile_options(waywallen-video-renderer PRIVATE
    -Wall -Wextra -Wpedantic
    -Wno-missing-field-initializers
)

target_link_libraries(waywallen-video-renderer
    PRIVATE
        waywallen::bridge
        wavsen::video
        wavsen::audio
        rstd::log
        Vulkan::Vulkan
        Threads::Threads
)

install(TARGETS waywallen-video-renderer
    RUNTIME DESTINATION ${CMAKE_INSTALL_BINDIR}
)

install(DIRECTORY ${CMAKE_CURRENT_SOURCE_DIR}/renderers/
    DESTINATION ${CMAKE_INSTALL_DATADIR}/waywallen/renderers
    FILES_MATCHING PATTERN "*.toml"
)

install(DIRECTORY ${CMAKE_CURRENT_SOURCE_DIR}/sources/
    DESTINATION ${CMAKE_INSTALL_DATADIR}/waywallen/sources
    FILES_MATCHING PATTERN "*.lua"
)
