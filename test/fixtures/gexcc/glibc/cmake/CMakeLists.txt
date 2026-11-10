cmake_minimum_required(VERSION 3.16)
project(findpkg NONE)
find_package(FooPkg CONFIG QUIET)
message(STATUS "FooPkg_FOUND=${FooPkg_FOUND}")
