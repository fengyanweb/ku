#ifndef KU_EXAMPLE_INTEROP_DOCUMENT_H
#define KU_EXAMPLE_INTEROP_DOCUMENT_H

#include "tinyxml2.h"

#include <stdexcept>

class ExampleDocument : public tinyxml2::XMLDocument {
public:
    void RaiseExampleError() const {
        throw std::runtime_error("example native exception");
    }
};

#endif
