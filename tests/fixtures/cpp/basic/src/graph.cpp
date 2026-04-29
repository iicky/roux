#include "graph.h"

namespace graph {

Node::Node(int id, std::string label) : id_(id), label_(std::move(label)) {}

int Node::id() const { return id_; }

const std::string& Node::label() const { return label_; }

Edge::Edge(int id, std::string label, int from, int to)
    : Node(id, std::move(label)), from_(from), to_(to) {}

int Edge::from() const { return from_; }

int Edge::to() const { return to_; }

template <typename T>
void Container<T>::add(const T& item) {
  items_.push_back(item);
}

template <typename T>
size_t Container<T>::size() const {
  return items_.size();
}

}  // namespace graph
